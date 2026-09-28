#!/usr/bin/env python3
"""Full-body Jellyfin music regression probe. Never prints credentials or media URLs."""
import argparse
import json
import pathlib
import subprocess
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--url', required=True)
    parser.add_argument('--token-file', required=True, type=pathlib.Path)
    parser.add_argument('--items-file', required=True, type=pathlib.Path,
                        help='JSON array of item IDs, or objects with id and optional duration_seconds')
    parser.add_argument('--output', required=True, type=pathlib.Path)
    parser.add_argument('--hls', action='store_true')
    args = parser.parse_args()
    token = args.token_file.read_text().strip()
    origin = urllib.parse.urlsplit(args.url)
    base = args.url.rstrip('/')
    headers = {'X-Emby-Token': token, 'User-Agent': 'RemuxMusicVerification/1'}
    items = json.loads(args.items_file.read_text())
    results = []

    class SameOriginRedirect(urllib.request.HTTPRedirectHandler):
        def redirect_request(self, req, fp, code, msg, hdrs, newurl):
            target = urllib.parse.urlsplit(newurl)
            if (target.scheme, target.netloc) != (origin.scheme, origin.netloc):
                raise ValueError('redirect unexpectedly leaves server origin')
            return super().redirect_request(req, fp, code, msg, hdrs, newurl)

    opener = urllib.request.build_opener(SameOriginRedirect())

    def request(url, extra=None):
        target = urllib.parse.urlsplit(url)
        if (target.scheme, target.netloc) != (origin.scheme, origin.netloc):
            raise ValueError('playlist unexpectedly redirects off server origin')
        return opener.open(urllib.request.Request(url, headers=headers | (extra or {})), timeout=30)

    def download(url, path):
        length = 0
        with request(url) as response, path.open('wb') as file:
            if response.status != 200:
                raise ValueError(f'full request HTTP {response.status}')
            expected = response.headers.get('Content-Length')
            while chunk := response.read(256 * 1024):
                length += len(chunk)
                if length > 512 * 1024 * 1024:
                    raise ValueError('body exceeds 512 MiB verification limit')
                file.write(chunk)
            if expected is not None and length != int(expected):
                raise ValueError('truncated response body')
        return length

    def decode(path, expected):
        run = subprocess.run(['ffmpeg', '-nostdin', '-v', 'error', '-xerror', '-i', str(path),
                              '-map', '0:a:0', '-f', 'null', '-'], capture_output=True, timeout=120)
        if run.returncode:
            raise ValueError('complete audio decode failed')
        probe = subprocess.run(['ffprobe', '-v', 'error', '-show_entries', 'format=duration',
                                '-of', 'json', str(path)], capture_output=True, check=True, timeout=30)
        duration = float(json.loads(probe.stdout)['format']['duration'])
        if expected and abs(duration - expected) > 5:
            raise ValueError('decoded duration does not match requested recording')
        return duration

    for value in items:
        item = {'id': value} if isinstance(value, str) else value
        started = time.monotonic()
        record = {'item_id': item['id'], 'route': 'audio-hls' if args.hls else 'item-file'}
        try:
            with tempfile.TemporaryDirectory(prefix='remux-music-proof-') as directory:
                root = pathlib.Path(directory)
                audio = root / 'audio'
                if args.hls:
                    url = f"{base}/audio/{item['id']}/main.m3u8"
                    deadline = time.monotonic() + 90
                    segments = {}
                    while True:
                        with request(url) as response:
                            url = response.geturl()
                            playlist = response.read(1024 * 1024).decode()
                        if '#EXTM3U' not in playlist:
                            raise ValueError('not an HLS playlist')
                        if '#EXT-X-STREAM-INF' in playlist:
                            raise ValueError('main.m3u8 incorrectly returned a master')
                        for line in playlist.splitlines():
                            if line and not line.startswith('#') and line not in segments:
                                path = root / f'segment-{len(segments)}.ts'
                                download(urllib.parse.urljoin(url, line), path)
                                segments[line] = path
                        if '#EXT-X-ENDLIST' in playlist:
                            break
                        if time.monotonic() >= deadline:
                            raise ValueError('HLS did not complete within 90 seconds')
                        time.sleep(0.3)
                    if not segments:
                        raise ValueError('empty HLS playlist')
                    with audio.open('wb') as joined:
                        for path in segments.values():
                            joined.write(path.read_bytes())
                    record['segments'] = len(segments)
                    record['bytes'] = audio.stat().st_size
                else:
                    url = f"{base}/items/{item['id']}/file"
                    record['bytes'] = download(url, audio)
                    for range_header in ['bytes=0-1', 'bytes=-64', f"bytes={record['bytes']//2}-"]:
                        with request(url, {'Range': range_header}) as response:
                            if response.status != 206 or not response.headers.get('Content-Range'):
                                raise ValueError('byte range contract failed')
                            data = response.read()
                        full = audio.read_bytes()
                        expected = full[:2] if range_header == 'bytes=0-1' else full[-64:] if range_header == 'bytes=-64' else full[len(full)//2:]
                        if data != expected:
                            raise ValueError('range bytes differ from complete object')
                    record['range_checks'] = 3
                record['decoded_seconds'] = decode(audio, item.get('duration_seconds'))
                record['passed'] = True
        except urllib.error.HTTPError as error:
            record.update(passed=False, error=f'HTTP {error.code}', retry_after=error.headers.get('Retry-After'))
        except Exception as error:
            # URLs and auth headers may occur in network exception strings.
            record.update(passed=False, error=type(error).__name__)
        record['elapsed_seconds'] = round(time.monotonic() - started, 3)
        results.append(record)
        print(json.dumps(record), flush=True)
        args.output.write_text(json.dumps({'results': results, 'all_passed': all(r['passed'] for r in results)}, indent=2) + '\n')
    raise SystemExit(0 if results and all(r['passed'] for r in results) else 1)


if __name__ == '__main__':
    main()
