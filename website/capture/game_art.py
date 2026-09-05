"""Original fictional in-game frames for the native picker, drawn with Pillow.

These are staged demo game content, not screenshots of third-party games.
No Orange UI is drawn here; GPUI alone renders the final app screenshots.
"""
from pathlib import Path
import math
import random
from PIL import Image, ImageDraw, ImageFont

W, H = 1280, 720
FONTS = Path("C:/Windows/Fonts")

def font(size, bold=False):
    return ImageFont.truetype(str(FONTS / ("segoeuib.ttf" if bold else "segoeui.ttf")), size)

def text(d, xy, value, size=20, fill="#eef8ff", bold=False):
    d.text(xy, value, font=font(size, bold), fill=fill, stroke_width=1, stroke_fill="#101b2a")

def sky(top, bottom):
    im = Image.new("RGB", (W,H))
    d = ImageDraw.Draw(im)
    for y in range(H):
        t=y/H
        d.line((0,y,W,y),fill=tuple(int(a+(b-a)*t) for a,b in zip(top,bottom)))
    return im

def finish(im, title, out):
    # Demo attribution is part of the source image, visible in the full preview.
    d=ImageDraw.Draw(im)
    text(d,(W-252,H-22),f"{title} / ORIGINAL DEMO",10,fill="#a7c3ce")
    im.save(out,optimize=True)

def arena(out):
    im=sky((17,29,49),(93,128,139));d=ImageDraw.Draw(im)
    rng=random.Random(18)
    # An arena camera, with all architecture projected from one world space.
    def p(x,y,z):
        return (int(640+610*x/(z+8)),int(325-610*(y-1.8)/(z+8)))
    def poly(points,color,line=None):
        points=[p(*v) for v in points];d.polygon(points,fill=color)
        if line:d.line(points+[points[0]],fill=line,width=2)
    poly([(-30,0,-5),(30,0,-5),(30,0,60),(-30,0,60)],"#354854")
    for z in range(-4,50,2):d.line((p(-30,0,z),p(30,0,z)),fill="#475f69",width=1)
    for x in range(-30,31,2):d.line((p(x,0,-5),p(x,0,60)),fill="#475f69",width=1)
    poly([(-8,0,28),(8,0,28),(8,8,28),(-8,8,28)],"#354b61","#648492")
    poly([(-8,0,-4),(-8,0,28),(-8,8,28),(-8,8,-4)],"#263849","#536c7c")
    poly([(8,0,-4),(8,0,28),(8,8,28),(8,8,-4)],"#3b5364","#627c89")
    # Tiled metal panels, light strips and service conduits.
    for side in [-8,8]:
        for z in range(-3,29,3):
            d.line((p(side,0,z),p(side,8,z)),fill="#152735",width=3)
        for y in [1,3,6]:d.line((p(side,y,-3),p(side,y,28)),fill="#192c3c",width=3)
        for z in [0,8,16,24]:
            d.line((p(side,5.5,z),p(side,5.5,z+2.5)),fill="#49e4ef",width=5)
    poly([(-3,0,27.9),(3,0,27.9),(3,4.5,27.9),(-3,4.5,27.9)],"#172631","#86adbb")
    poly([(-2.6,0,27.8),(2.6,0,27.8),(2.6,4.1,27.8),(-2.6,4.1,27.8)],"#1f3440")
    d.line((p(0,0,27.7),p(0,4.1,27.7)),fill="#77f1ef",width=3)
    # Overhead gantry and bright opening give the map depth.
    poly([(-8,5.8,13),(8,5.8,13),(8,6.7,13),(-8,6.7,13)],"#1a2a37","#718a97")
    d.line((p(-7,5.75,13),p(7,5.75,13)),fill="#fda967",width=4)
    def box(x,z,w,h,depth,col):
        # Front, right and top faces; architectural cover, not flat rectangles.
        poly([(x,0,z),(x+w,0,z),(x+w,h,z),(x,h,z)],col,"#13232e")
        poly([(x+w,0,z),(x+w,0,z+depth),(x+w,h,z+depth),(x+w,h,z)],"#253e48","#13232e")
        poly([(x,h,z),(x+w,h,z),(x+w,h,z+depth),(x,h,z+depth)],"#6a8186","#a0b4ad")
        for y in [.15,h-.15]:d.line((p(x,y,z-.01),p(x+w,y,z-.01)),fill="#96a89b",width=2)
        for a in [.15,w-.15]:d.line((p(x+a,.1,z-.02),p(x+a,h-.1,z-.02)),fill="#20313a",width=3)
        if w>1.5:
            for j in range(5):
                xx=x+.4+j*.25
                d.line((p(xx,.25,z-.03),p(xx+.22,.52,z-.03)),fill="#dfa457",width=3)
    box(-5,16,2.4,2.2,2.8,"#537277")
    box(3,10,2.8,1.9,2.2,"#49656b")
    box(-6,2,3.1,1.4,2.8,"#58645d")
    # Scuff marks and floor debris: deterministic detail in perspective.
    for _ in range(110):
        x=rng.uniform(-7.6,7.6);z=rng.uniform(-2,27)
        a=p(x,.01,z);b=p(x+.12,.01,z+.08)
        d.line((a,b),fill=rng.choice(["#617578","#2b3d46","#83908a"]),width=1)
    # Capture zone projected onto the arena floor.
    ring=[p(2.1*math.cos(a),.02,15+2.1*math.sin(a)) for a in [i*math.pi/24 for i in range(49)]]
    d.line(ring,fill="#38ddf1",width=4)
    x,y=p(0,2.6,15)
    d.polygon([(x,y-30),(x+21,y-9),(x,y+12),(x-21,y-9)],fill="#163845",outline="#68edff",width=3)
    text(d,(x-8,y-26),"B",20,bold=True)
    text(d,(x-45,y-60),"CAPTURE",13,fill="#75ecf4",bold=True)
    # A small distant opposing player in armour.
    x,y=p(2.1,0,19);s=610/27
    d.ellipse((x-7,y-int(1.85*s),x+7,y-int(1.3*s)),fill="#db7864",outline="#ffc398",width=2)
    d.polygon([(x-11,y-31),(x+11,y-31),(x+15,y-13),(x+7,y-10),(x+4,y-1),(x-3,y-1),(x-7,y-13),(x-15,y-17)],fill="#a44340",outline="#f6906c")
    d.line((x+8,y-27,x+24,y-24),fill="#202632",width=6)
    # First-person hands and modular rifle occupy the lower-right foreground.
    d.polygon([(786,720),(789,628),(868,582),(931,596),(1050,720)],fill="#202b33",outline="#72928d",width=3)
    d.polygon([(873,627),(915,571),(980,586),(999,663),(950,720),(899,686)],fill="#747964",outline="#c0b49a",width=3)
    d.polygon([(864,590),(812,483),(825,466),(876,490),(1008,533),(1219,720),(1019,720)],fill="#172732",outline="#a2b7b9",width=3)
    d.polygon([(827,468),(851,447),(913,470),(942,518),(873,509)],fill="#405967",outline="#90aab1",width=2)
    d.polygon([(856,461),(861,437),(889,438),(904,481)],fill="#0c202d",outline="#d1e5d9",width=3)
    d.polygon([(882,516),(924,490),(1131,612),(1073,665)],fill="#3e5664",outline="#78929c",width=3)
    d.polygon([(930,510),(993,547),(996,565),(940,535)],fill="#43d9e6")
    for j in range(7):
        d.line((952+j*17,567+j*11,944+j*17,594+j*11),fill="#13252e",width=7)
    d.polygon([(1014,633),(1085,609),(1230,706),(1210,720),(1102,715)],fill="#243541",outline="#728d99",width=3)
    d.rectangle((935,554,992,580),fill="#112a37",outline="#5d909b")
    text(d,(946,553),"24",18,fill="#8be7ec",bold=True)
    # Crisp in-game HUD, not a title-card overlay.
    hud=Image.new("RGBA",im.size);hd=ImageDraw.Draw(hud)
    hd.rounded_rectangle((480,22,800,84),radius=8,fill=(9,18,29,210))
    hd.rectangle((480,22,565,84),fill=(29,127,156,220));hd.rectangle((715,22,800,84),fill=(174,68,64,220))
    text(hd,(501,26),"42",34,bold=True);text(hd,(736,26),"37",34,bold=True)
    text(hd,(597,27),"04:28",27,bold=True);text(hd,(598,61),"CONTROL",12,fill="#90bac9")
    hd.rounded_rectangle((26,22,187,182),radius=8,fill=(9,18,29,190),outline=(140,187,199,200),width=2)
    for a in [(45,45,80,80),(115,46,166,63),(51,111,95,153),(125,97,160,157)]:hd.rectangle(a,fill="#4e6572")
    hd.line((93,40,93,99,114,99,114,167),fill="#7a9cab",width=4)
    hd.polygon([(103,112),(96,128),(110,128)],fill="#70f5ea")
    hd.ellipse((145,73,152,80),fill="#fb7563")
    text(hd,(29,194),"VECTOR ARENA",17,bold=True)
    text(hd,(1021,29),"fragbyte  +  125",16,fill="#a2f6ed")
    text(hd,(26,641),"100",43,bold=True);text(hd,(118,660),"HP  /  50 ARMOR",17,fill="#a7d3df")
    hd.rectangle((29,699,242,704),fill="#59dce0")
    text(hd,(1090,628),"24",52,bold=True);text(hd,(1176,658),"/ 96",23,fill="#adc4cb")
    text(hd,(1094,687),"PULSE RIFLE",13,fill="#d0dde1")
    for a in [(630,350,636,350),(644,350,650,350),(640,340,640,346),(640,354,640,360)]:hd.line(a,fill="#effffc",width=2)
    im=Image.alpha_composite(im.convert("RGBA"),hud).convert("RGB")
    finish(im,"VECTOR ARENA",out/"arena.png")

def racing(out):
    im=sky((29,35,74),(239,126,89));d=ImageDraw.Draw(im);rng=random.Random(7)
    d.ellipse((885,70,1020,205),fill="#ffd4a2")
    for layer,col in [(0,"#60566f"),(1,"#3e435e")]:
        pts=[(0,400)]+[(x,220+layer*65+rng.randint(-75,40)) for x in range(0,1400,90)]+[(1280,500)]
        d.polygon(pts,fill=col)
    # Neon circuit in perspective, curving right at the horizon.
    for y in range(305,720):
        t=(y-305)/415;center=640+160*(1-t)**3;half=32+600*t*t
        d.line((0,y,1280,y),fill=(37+int(t*13),47+int(t*8),58+int(t*5)))
        d.line((center-half,y,center+half,y),fill=(54+int(t*9),59+int(t*9),71+int(t*9)))
        for side in [-1,1]:
            edge=center+side*half;stripe=4+18*t
            d.line((edge-stripe,y,edge+stripe,y),fill="#e76e69" if int(t*28)%2 else "#e7ded0")
        if int(t*35)%4<2:
            for lane in [-.33,.33]:
                x=center+lane*half;d.line((x-2-t*3,y,x+2+t*3,y),fill="#d4cec0")
    for t in [.12,.21,.34,.52,.75,1.05]:
        y=305+415*t;center=640+160*(1-t)**3;half=32+600*t*t
        for side in [-1,1]:
            x=center+side*(half+40*t);height=130*t
            d.line((x,y,x,y-height),fill="#303745",width=max(2,int(8*t)))
            d.line((x,y-height,x-side*80*t,y-height-8*t),fill="#65e4eb",width=max(2,int(5*t)))
    # Distant opponent car.
    d.rounded_rectangle((750,342,791,365),radius=4,fill="#d5ae55",outline="#252e3d",width=2)
    d.rectangle((758,340,781,350),fill="#344d62")
    # Third-person player's sports car, rear perspective.
    d.ellipse((410,618,869,706),fill="#222a36")
    d.rounded_rectangle((442,535,490,670),radius=15,fill="#111e2a")
    d.rounded_rectangle((789,535,839,670),radius=15,fill="#111e2a")
    d.polygon([(462,558),(510,452),(765,452),(817,558),(842,641),(812,674),(468,674),(438,641)],fill="#dd673d",outline="#f8b57f",width=3)
    d.polygon([(510,452),(544,419),(734,419),(765,452),(784,546),(491,546)],fill="#ee814d",outline="#ffc38d",width=3)
    d.polygon([(549,432),(730,432),(751,517),(527,517)],fill="#263c53",outline="#a4b7be",width=3)
    d.polygon([(551,437),(608,437),(586,512),(534,512)],fill="#445773")
    d.polygon([(452,578),(827,578),(809,652),(469,652)],fill="#a94437")
    d.rectangle((462,574,818,595),fill="#182b3b",outline="#97a8ac",width=2)
    d.rectangle((463,596,552,611),fill="#ff454b");d.rectangle((728,596,817,611),fill="#ff454b")
    d.rectangle((588,617,690,644),fill="#25303e");text(d,(604,619),"P1XEL",16,bold=True)
    d.line((563,429,550,548),fill="#152839",width=12);d.line((720,429,733,548),fill="#152839",width=12)
    d.rectangle((478,547,800,558),fill="#222e3f")
    for x in [494,778]:d.ellipse((x-12,635,x+12,654),fill="#0a1823",outline="#87959d",width=2)
    text(d,(30,22),"NIGHT CIRCUIT",24,bold=True)
    text(d,(30,65),"POSITION",14,fill="#aec8d8");text(d,(30,83),"02 / 08",39,bold=True)
    text(d,(1060,25),"LAP  2 / 3",27,bold=True);text(d,(1050,68),"01:24.680",25,fill="#8ff2ee")
    text(d,(35,624),"BEST LAP",16,fill="#afc5d4");text(d,(35,649),"01:19.402",30,bold=True)
    d.arc((1004,490,1237,723),180,354,fill="#466175",width=12)
    d.arc((1004,490,1237,723),180,308,fill="#67e8e5",width=12)
    text(d,(1058,572),"218",58,bold=True);text(d,(1090,638),"KM/H",17,fill="#a2c5d6")
    text(d,(979,651),"5",39,fill="#ffb777",bold=True)
    finish(im,"NIGHT CIRCUIT",out/"racing.png")

def voxel(out):
    im=sky((79,137,181),(176,214,206));d=ImageDraw.Draw(im)
    d.rectangle((1030,64,1111,145),fill="#ffe9a5")
    # Isometric grass/stone terrain built from individually shaded cubes.
    def cube(x,y,s,top,left,right,h=None):
        h=h or s
        d.polygon([(x,y),(x+s,y-s//2),(x+2*s,y),(x+s,y+s//2)],fill=top)
        d.polygon([(x,y),(x+s,y+s//2),(x+s,y+s//2+h),(x,y+h)],fill=left)
        d.polygon([(x+s,y+s//2),(x+2*s,y),(x+2*s,y+h),(x+s,y+s//2+h)],fill=right)
    for row in range(18):
        for col in range(23):
            x=col*64-row*32-220;y=220+row*22
            wet=col>17 or (row<5 and col>10)
            cube(x,y,32,"#6fbabc" if wet else "#7d9d55","#4b7475" if wet else "#766146","#4d9194" if wet else "#5f533c",38)
    for x,y in [(280,254),(91,361),(790,260),(947,347),(524,191)]:
        cube(x+45,y,19,"#947951","#68523c","#524333",114)
        for xx,yy in [(0,0),(35,-17),(69,0),(35,16),(35,-47)]:cube(x+xx,y+yy-68,39,"#7d9956","#436442","#375b3b",47)
    # Small base with stone walls, lit doorway and a workbench.
    for row in range(3):
        for col in range(4):cube(398+col*44,380-row*42,22,"#9caa9b","#71837b","#586e69",42)
    for row in range(3):
        for col in range(3):cube(574+col*44,380-col*22-row*42,22,"#a2b2a0","#71837b","#586e69",42)
    d.polygon([(470,378),(517,397),(517,471),(470,448)],fill="#27352f")
    d.rectangle((483,389,491,416),fill="#ffbe5f")
    cube(620,440,39,"#b39663","#7f5d3e","#614934",38)
    # Foreground voxel pickaxe, deliberately unmistakable survival gameplay.
    d.polygon([(995,709),(1023,720),(1157,484),(1127,473)],fill="#956b45",outline="#d0a375",width=5)
    d.polygon([(1030,459),(1082,420),(1167,439),(1225,499),(1187,530),(1142,476),(1089,466),(1054,489)],fill="#88c5c3",outline="#284d56",width=6)
    d.rectangle((626,350,651,353),fill="#ffffff");d.rectangle((637,339,640,364),fill="#ffffff")
    text(d,(27,23),"BLOCKLANDS",25,bold=True);text(d,(29,62),"SURVIVAL / DAY 12",17,fill="#e2f5db")
    text(d,(963,26),"BUILD A SHELTER",20,bold=True);text(d,(1030,58),"Wood  24 / 32",17)
    for i in range(10):
        x=360+i*27;d.polygon([(x,619),(x+6,613),(x+12,617),(x+18,613),(x+24,619),(x+12,634)],fill="#e16956",outline="#432c2d")
    for i in range(9):
        x=344+i*65;d.rectangle((x,650,x+61,710),fill="#3b4845",outline="#eef0c6" if i==0 else "#71877d",width=4)
        if i in [1,2,4,6]:cube(x+10,670,16,"#baa67d","#887650","#645b41",17)
        elif i==0:d.line((x+16,695,x+43,662),fill="#9acecb",width=8)
        text(d,(x+40,686),str([1,32,18,0,24,0,8,0,0][i]) if i<7 else "",12)
    finish(im,"BLOCKLANDS",out/"voxel.png")

def rpg(out):
    im=sky((17,27,38),(48,54,49));d=ImageDraw.Draw(im)
    for x in range(0,1280,75):
        d.polygon([(x,720),(x+10,70),(x+32,0),(x+50,720)],fill="#192f31")
    d.rounded_rectangle((45,43,1235,662),radius=8,fill="#202a30",outline="#9c9368",width=3)
    text(d,(74,62),"RUNEVALE",27,bold=True);text(d,(518,67),"INVENTORY",26,fill="#e9d7a6",bold=True)
    text(d,(77,114),"pixelpilot",23,bold=True);text(d,(77,149),"LEVEL 24  /  RANGER",15,fill="#b7c8b1")
    # Armoured character equipment preview.
    d.ellipse((145,524,418,576),fill="#152126")
    d.polygon([(214,234),(335,234),(361,394),(400,508),(153,508),(188,394)],fill="#496d66",outline="#879386",width=3)
    d.polygon([(228,354),(270,369),(260,511),(233,550),(201,548)],fill="#647b76",outline="#aab0a0",width=3)
    d.polygon([(281,369),(324,354),(348,548),(312,550),(284,511)],fill="#647b76",outline="#aab0a0",width=3)
    d.polygon([(192,253),(230,230),(265,271),(253,350),(213,346),(180,309)],fill="#92a09a",outline="#d8d4b0",width=3)
    d.polygon([(282,271),(327,230),(362,253),(373,309),(337,346),(290,350)],fill="#788d87",outline="#d8d4b0",width=3)
    d.polygon([(238,254),(228,203),(248,176),(295,176),(318,203),(308,254),(273,271)],fill="#91a49b",outline="#dbd8b6",width=3)
    d.polygon([(242,212),(302,212),(293,232),(251,232)],fill="#1b3036")
    d.line((184,280,146,457),fill="#c2ac77",width=8);d.arc((98,260,190,480),270,90,fill="#d5bf8a",width=7)
    d.line((144,262,146,478),fill="#c1d2ba",width=2)
    text(d,(97,590),"HP  420 / 420",21,fill="#b9dac2");text(d,(302,590),"ARMOR  168",21,fill="#c8c2a4")
    for row in range(4):
        for col in range(7):
            x=503+col*96;y=137+row*101
            d.rounded_rectangle((x,y,x+85,y+86),radius=3,fill="#29393d",outline="#85765b" if row<2 else "#465554",width=2)
            k=row*7+col
            if k%4==0:
                d.line((x+24,y+65,x+64,y+19),fill="#b9dbd3",width=8);d.line((x+23,y+44,x+44,y+65),fill="#d2a867",width=6)
            elif k%4==1:
                d.ellipse((x+22,y+25,x+63,y+67),fill="#ba6554",outline="#ead4a0",width=3);d.rectangle((x+34,y+15,x+51,y+30),fill="#b1c0a5")
            elif k%4==2:
                d.polygon([(x+42,y+16),(x+66,y+37),(x+53,y+66),(x+31,y+66),(x+18,y+37)],fill="#819b87",outline="#bdcca6",width=3)
            else:
                d.arc((x+21,y+15,x+67,y+69),220,520,fill="#dab376",width=10)
            text(d,(x+66,y+66),str(1+k%8),12)
    text(d,(508,565),"WINDRUNNER BOW",23,fill="#dac58d",bold=True)
    text(d,(510,604),"Rare  /  +42 agility",19,fill="#9dc7ba")
    text(d,(1030,599),"8,420 GOLD",21,fill="#ebc77d",bold=True)
    finish(im,"RUNEVALE",out/"rpg.png")

def generate(out):
    out=Path(out);out.mkdir(exist_ok=True,parents=True)
    arena(out);racing(out);voxel(out);rpg(out)
    # A staged game-only desktop, with no real Windows desktop content.
    im=Image.new("RGB",(1280,720),"#172938")
    for name,pos,bounds in [("arena.png",(25,27),(825,464)),("racing.png",(873,69),(380,214)),("voxel.png",(873,317),(380,214))]:
        game=Image.open(out/name);game.thumbnail(bounds,Image.Resampling.LANCZOS);im.paste(game,pos)
    d=ImageDraw.Draw(im);d.rounded_rectangle((422,653,856,701),radius=12,fill="#253d4a")
    for i,c in enumerate(["#db854d","#66c7cd","#bce092","#cabc8c"]):d.rectangle((454+i*98,666,478+i*98,690),fill=c)
    im.save(out/"desktop.png",optimize=True)

if __name__=="__main__":
    import sys
    generate(Path(sys.argv[1]))
