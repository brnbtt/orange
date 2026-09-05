"""Seed only a disposable linked worktree; renderers are never modified."""
from pathlib import Path
import argparse
import shutil
import sys

sys.dont_write_bytecode = True
from game_art import generate

p = argparse.ArgumentParser()
p.add_argument("worktree", type=Path)
args = p.parse_args()
here = Path(__file__).resolve().parent
root = args.worktree.resolve()
if root == here.parent.parent or not (root / ".git").is_file():
    raise SystemExit("Use a separate disposable linked worktree, not a main checkout")
source = root / "crates/orange-client/src/main.rs"
text = source.read_text(encoding="utf-8")
assert "mod capture_fixture;" not in text, "Fixture already applied"
for anchor in ["mod background;", "fn main() {", "    fn code(&self) -> Option<String> {", "\n}\n\nimpl Drop for Orange"]:
    if text.count(anchor) != 1:
        raise SystemExit(f"Capture fixture no longer matches this revision: {anchor!r}")
text = text.replace("mod background;", "mod capture_fixture;\nmod background;", 1)
text = text.replace("fn main() {", "fn main() { capture_fixture::run(); }\n\n#[allow(dead_code)]\nfn production_main() {", 1)
start = text.index("    fn code(&self) -> Option<String> {")
end = text.index("\n}\n\nimpl Drop for Orange", start)
text = text[:start] + '''    fn code(&self) -> Option<String> {
        (self.screen == Screen::Streaming).then(|| "ORA-NGE".into())
    }

    fn viewers(&self) -> Vec<String> {
        if self.screen == Screen::Streaming { vec!["fragbyte".into(), "nightshift".into()] }
        else { Vec::new() }
    }
''' + text[end:]
source.write_text(text, encoding="utf-8")
shutil.copyfile(here / "fixture.rs", root / "crates/orange-client/src/capture_fixture.rs")
out = root / "capture-art"
generate(out)
print(f"Fixture applied in {root}; original demo game artwork in {out}")
