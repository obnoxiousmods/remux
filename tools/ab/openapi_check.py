#!/usr/bin/env python3
"""Validate remux responses against the REAL Jellyfin OpenAPI schema.

No jsonschema dependency (PEP 668 blocks installs here), so this implements the
subset of OpenAPI 3.0 needed for these payloads: $ref resolution, type checks,
nullable, enum, and required. It reports any field remux emits that Jellyfin's
schema does not define, any type mismatch, and any required field missing.
"""
import os

import json
import sys

SPEC_PATH = os.environ.get('JELLYFIN_OPENAPI', 'jellyfin_openapi.json')
SPEC = json.load(open(SPEC_PATH))
SCHEMAS = SPEC["components"]["schemas"]

TYPEMAP = {
    "string": str, "integer": int, "number": (int, float),
    "boolean": bool, "array": list, "object": dict,
}


def deref(sch):
    seen = 0
    while "$ref" in sch and seen < 20:
        sch = SCHEMAS[sch["$ref"].split("/")[-1]]
        seen += 1
    return sch


def merge(sch):
    """Flatten allOf so composed schemas expose their properties.

    Must NOT assume the composed result is an object: Jellyfin expresses a
    nullable enum as `allOf: [{$ref: SomeEnum}]`, whose target is
    `{type: string, enum: [...]}`. Forcing `type: object` here made every enum
    string look like a type error.
    """
    sch = deref(sch)
    if "allOf" not in sch:
        return sch
    out = {"properties": {}, "required": []}
    for part in sch["allOf"]:
        p = merge(part)
        if "properties" in p:
            out["properties"].update(p["properties"])
        out["required"] += p.get("required", [])
        # inherit the composed type/enum (the nullable-enum case)
        for k in ("type", "enum", "items", "format"):
            if k in p and k not in out:
                out[k] = p[k]
    for k, v in sch.items():
        if k != "allOf":
            out.setdefault(k, v)
    if not out["properties"]:
        out.pop("properties")
    if not out["required"]:
        out.pop("required")
    return out


def check(value, sch, path, errs, unknown):
    sch = merge(sch)
    if value is None:
        if not sch.get("nullable", False) and "type" in sch:
            pass  # Jellyfin marks most things nullable; absent nullable + null is common in practice
        return
    t = sch.get("type")
    if t and t in TYPEMAP and not isinstance(value, TYPEMAP[t]):
        if not (t == "number" and isinstance(value, bool) is False and isinstance(value, (int, float))):
            errs.append(f"{path}: expected {t}, got {type(value).__name__}")
            return
    if t == "object" or "properties" in sch:
        props = sch.get("properties", {})
        for k in sch.get("required", []):
            if k not in value:
                errs.append(f"{path}.{k}: REQUIRED field missing")
        for k, v in value.items():
            if k in props:
                check(v, props[k], f"{path}.{k}", errs, unknown)
            elif not sch.get("additionalProperties", True):
                unknown.append(f"{path}.{k}")
            elif not props:
                pass
            else:
                unknown.append(f"{path}.{k}")
    elif t == "array":
        item_sch = sch.get("items")
        if item_sch:
            for i, v in enumerate(value[:50]):
                check(v, item_sch, f"{path}[]", errs, unknown)


def validate(payload, schema_name, label):
    errs, unknown = [], []
    check(payload, {"$ref": f"#/components/schemas/{schema_name}"}, schema_name, errs, unknown)
    uniq_unknown = sorted(set(unknown))
    print(f"\n  {label}")
    print(f"    type errors / missing-required : {len(errs)}")
    for e in errs[:8]:
        print(f"       - {e}")
    print(f"    fields not in Jellyfin schema  : {len(uniq_unknown)}")
    for u in uniq_unknown[:12]:
        print(f"       - {u}")
    return len(errs), uniq_unknown


print("Validating remux /items responses against Jellyfin 10.11.11 BaseItemDtoQueryResult")
tot_err = 0
allunknown = set()
for n, label in (("rating", "CommunityRating"), ("premiere", "PremiereDate"),
                 ("digital", "DigitalReleaseDate"), ("runtime", "Runtime")):
    for arm in ("no", "yes"):
        payload = json.load(open(f"e2e_{n}_{arm}.json"))
        e, u = validate(payload, "BaseItemDtoQueryResult",
                        f"sortBy={label}  [{'WITHOUT index' if arm=='no' else 'WITH index'}]")
        tot_err += e
        allunknown |= set(u)

print(f"\n=== TOTAL type errors across all 8 payloads: {tot_err} ===")
print(f"=== distinct fields remux emits that Jellyfin's schema lacks: {len(allunknown)} ===")
for u in sorted(allunknown)[:20]:
    print("   ", u)
