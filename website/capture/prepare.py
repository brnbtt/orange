"""Seed a disposable Orange worktree; never modify the website's source crates."""
from pathlib import Path
import argparse
import shutil
from PIL import Image, ImageDraw, ImageFont

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
        if self.screen == Screen::Streaming { vec!["Maya".into(), "Sam".into()] }
        else { Vec::new() }
    }
''' + text[end:]
source.write_text(text, encoding="utf-8")
shutil.copyfile(here / "fixture.rs", root / "crates/orange-client/src/capture_fixture.rs")

out = root / "capture-art"
out.mkdir(exist_ok=True)
fontdir = Path("C:/Windows/Fonts")
def font(n, bold=False):
    return ImageFont.truetype(str(fontdir / ("segoeuib.ttf" if bold else "segoeui.ttf")), n)

# Original geometry and fictional text only; these are input thumbnails, not app UI.
im = Image.new("RGB", (960,540)); d = ImageDraw.Draw(im)
for y in range(540):
    t = y / 540
    d.line((0,y,960,y), fill=(int(31+105*t),int(45+38*t),int(80+20*t)))
d.ellipse((670,80,796,206), fill="#ffc687")
d.polygon([(0,255),(140,173),(285,278),(422,198),(560,286),(720,216),(960,280),(960,540),(0,540)],fill="#354963")
d.polygon([(0,328),(150,258),(305,338),(510,271),(700,350),(960,296),(960,540),(0,540)],fill="#273f55")
d.polygon([(0,382),(960,339),(960,540),(0,540)],fill="#56848d")
for y in range(370,520,14):
    x=680-(y-370)//2
    d.line((x,y,x+100+(y-370),y),fill="#aeb09b",width=2)
d.polygon([(0,442),(172,374),(283,404),(409,473),(530,540),(0,540)],fill="#1d303c")
d.polygon([(143,394),(186,244),(208,394)],fill="#1b353c")
d.polygon([(100,369),(185,269),(260,369)],fill="#1b353c")
d.rounded_rectangle((32,28,280,82),radius=12,fill="#1f2d42")
d.text((49,39),"COASTLINE",font=font(25,True),fill="#faf1dc")
d.text((35,468),"Take the scenic route.",font=font(27,True),fill="#fce9cf")
d.text((36,506),"EVENING SESSION  /  WIND DOWN",font=font(12),fill="#c5d5d4")
im.save(out / "coastline.png")

im=Image.new("RGB",(960,540),"#eee9de");d=ImageDraw.Draw(im)
d.rectangle((0,0,960,52),fill="#fbf8f0")
d.text((25,12),"Weekend ideas",font=font(22,True),fill="#393c3a")
for box,color,title,lines in [((40,92,302,302),"#f9c696","MAKE SOMETHING",["A tiny co-op game","A bright new world"]),((340,150,610,382),"#b9d8cf","PLAY TOGETHER",["Saturday, 8 pm","Bring your ideas"]),((650,84,918,294),"#d2c9eb","KEEP IT SIMPLE",["One window","Good company"])]:
    d.rounded_rectangle(box,radius=8,fill=color)
    x,y,_,_=box;d.text((x+20,y+24),title,font=font(17,True),fill="#434347")
    for i,line in enumerate(lines):d.text((x+20,y+70+i*30),line,font=font(18),fill="#434347")
d.line((196,322,196,432,748,432,748,315),fill="#9d988c",width=3)
d.text((386,469),"A little room for possibility.",font=font(19),fill="#76736d")
im.save(out / "canvas.png")

im=Image.new("RGB",(960,540),"#19212e");d=ImageDraw.Draw(im)
d.rectangle((0,0,960,44),fill="#232e3e");d.text((20,9),"Little orbit / prototype",font=font(19),fill="#dde5ef")
d.rectangle((0,44,200,540),fill="#202938")
for i,line in enumerate(["PROJECT","  scenes","    orbit.scene","  scripts","    movement","    stars","  audio"]):d.text((18,75+i*38),line,font=font(17),fill="#9dacc1")
lines=[("01  // A small world to explore", "#80949b"),("02  fn update(world) {", "#cba9db"),("03      orbit.rotate(0.02);", "#c0d8ef"),("04      stars.twinkle();", "#c0d8ef"),("05      player.follow(cursor);", "#c0d8ef"),("06  }", "#cba9db")]
for i,(line,color) in enumerate(lines):d.text((227,91+i*38),line,font=font(20),fill=color)
d.rounded_rectangle((229,369,910,489),radius=9,fill="#24374b")
d.ellipse((730,388,810,468),fill="#df986a");d.arc((676,410,862,449),0,360,fill="#accad9",width=3)
d.text((251,394),"PLAYTEST READY",font=font(20,True),fill="#8bd5bc")
d.text((252,436),"Build 014  /  all systems go",font=font(17),fill="#9dacc1")
im.save(out / "editor.png")

im=Image.new("RGB",(960,540),"#252a34");d=ImageDraw.Draw(im)
d.rectangle((0,0,960,55),fill="#303641");d.text((28,14),"Sunday mix",font=font(22,True),fill="#f1eee8")
d.rounded_rectangle((40,95,340,395),radius=15,fill="#c9906b")
for r,col in [(110,"#33364b"),(77,"#c1d2c6"),(40,"#c9906b")]:d.ellipse((190-r,245-r,190+r,245+r),fill=col)
d.text((385,113),"For the slow afternoons",font=font(29,True),fill="#f1eee8")
for i,line in enumerate(["01   A little daylight", "02   Open windows", "03   Somewhere by the sea", "04   Home before dark"]):d.text((385,186+i*49),line,font=font(22),fill="#afb9c5")
d.line((42,466,915,466),fill="#505664",width=6);d.line((42,466,429,466),fill="#d7a07a",width=6)
d.text((42,490),"02:14",font=font(16),fill="#bfc7cf");d.text((870,490),"04:58",font=font(16),fill="#bfc7cf")
im.save(out / "music.png")

im=Image.new("RGB",(960,540),"#263a4b")
for name,box in [("coastline.png",(35,38,563,335)),("canvas.png",(589,90,929,281))]:
    art=Image.open(out/name);art.thumbnail((box[2]-box[0],box[3]-box[1]));im.paste(art,box[:2])
d=ImageDraw.Draw(im);d.rounded_rectangle((290,486,670,526),radius=16,fill="#172532")
for x,c in [(319,"#e9a36f"),(375,"#adc8d3"),(431,"#c8b5df"),(487,"#a8cbb7"),(543,"#dba894"),(599,"#c3cdd9")]:d.rounded_rectangle((x,496,x+22,518),radius=5,fill=c)
im.save(out / "desktop.png")
print(f"Fixture applied in {root}; original artwork in {out}")
