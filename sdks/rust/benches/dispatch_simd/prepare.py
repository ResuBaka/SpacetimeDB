"""Build a standalone, capture-free table-dispatch benchmark from local Rust bindings."""

import argparse
import json
import re
import shutil
from pathlib import Path


def schema_names(bindings):
    source = bindings.read_text()
    # Read the generated transaction parser, not the set of names in the
    # capture: cold tables also affect the code and size of the dispatch table.
    if "fn __lookup_table_name(" in source:
        entries = re.search(r'const ENTRIES:\s*&\[\(&str,\s*usize\)\]\s*=\s*&\[(.*?)\];', source, re.DOTALL)
        if not entries:
            raise ValueError("could not read the generated table name index")
        pairs = re.findall(r'\("([^"\n]*)",\s*(\d+)\)', entries[1])
        names = [n for n, _ in sorted(pairs, key=lambda p: int(p[1]))]
    else:
        start = source.index("match &table_update.table_name[..] {")
        end = source.index("unknown =>", start)
        names = re.findall(r'^\s*"([^"\n]*)"\s*=>', source[start:end], re.MULTILINE)
    if not names or len(set(names)) != len(names) or not all(re.fullmatch(r"[a-zA-Z0-9_]+", n) for n in names):
        raise ValueError("expected distinct, unescaped ASCII table names in the generated Rust transaction parser")
    return names


def prepare(bindings, dest):
    names = schema_names(bindings)
    if len(names) >= 65535:
        raise ValueError("benchmark IDs require fewer than 65535 tables")
    here = Path(__file__).resolve().parent
    crates = here.parents[3] / "crates"
    groups = {}
    for index, name in enumerate(names):
        groups.setdefault(len(name), []).append(index)
    ids, tags, offsets = [], [], []
    for length in range(max(groups) + 1):
        bucket = groups.get(length, [])
        offsets.append(f"({len(ids)},{len(bucket)})")
        for index in bucket:
            b = names[index].encode()
            tag = bytes((b[0], b[length // 2], b[length // 3], b[max(0, length - 2)]))
            tags.append(int.from_bytes(tag, "little"))
            ids.append(index)
        padding = -len(bucket) % 8
        ids.extend([65535] * padding)
        tags.extend([0] * padding)

    def array(name, ty, values):
        return f"const {name}:[{ty};{len(values)}]=[{','.join(map(str, values))}];\n"

    tables = array("NAMES", "&str", [json.dumps(n) for n in names])
    tables += array("TAGS", "u32", tags) + array("IDS", "u16", ids) + array("OFF", "(usize,usize)", offsets)
    tables += "#[inline(never)]fn generated(n:&str)->Option<u16>{match n{\n"
    tables += "".join(f"{json.dumps(n)}=>Some({i}),\n" for i, n in enumerate(names)) + "_=>None,}}\n"
    dest.mkdir(parents=True, exist_ok=False)
    (dest / "src").mkdir()
    (dest / "src/schema.rs").write_text(tables)
    shutil.copyfile(here / "replay.rs", dest / "src/main.rs")
    (dest / "Cargo.toml").write_text(
        '[workspace]\n[package]\nname="table-dispatch-replay"\nversion="0.0.0"\nedition="2024"\n'
        '[profile.release]\nlto="thin"\ncodegen-units=16\n[dependencies]\nfearless_simd="=1.2.0"\n'
        + "".join(
            f"spacetimedb-{name}={{path={json.dumps(str(crates / name))}}}\n"
            for name in ["sats", "client-api-messages"]
        )
    )
    print(f"Prepared benchmark with {len(names)} schema names in {dest}")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("bindings", type=Path, help="module_bindings/mod.rs from the matching generated Rust client")
    parser.add_argument("dest", type=Path, help="new output directory (must not already exist)")
    args = parser.parse_args()
    prepare(args.bindings, args.dest)
