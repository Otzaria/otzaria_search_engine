import subprocess
import tomllib
from pathlib import Path

old = tomllib.loads(subprocess.check_output(["git", "show", "13f19d11eb5a4ba4c2e04a5c097e7500b4e6e827:rust/Cargo.lock"], text=True))
new = tomllib.loads(Path("rust/Cargo.lock").read_text())
new_revision = "b6dfc7bc93336df1b996a6701b8c86c80206b8d1"

def normalized(lock):
    packages = []
    for package in lock["package"]:
        package = dict(package)
        if package.get("source", "").startswith("git+https://github.com/Otzaria/tantivy?"):
            package["source"] = "TANTIVY_PIN"
        packages.append(package)
    return sorted(packages, key=lambda item: (item["name"], item["version"], item.get("source", "")))

assert normalized(old) == normalized(new), "The update changed dependencies beyond the Tantivy source revision"
sources = [p["source"] for p in new["package"] if p.get("source", "").startswith("git+https://github.com/Otzaria/tantivy?")]
assert len(sources) == 9, sources
assert all(source.endswith("#" + new_revision) and ("rev=" + new_revision) in source for source in sources)
print("Verified: all nine Tantivy workspace sources use the fixed commit; every other package and dependency edge is unchanged")
