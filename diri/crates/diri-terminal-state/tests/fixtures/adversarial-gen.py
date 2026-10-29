import os, sys, base64, random, zlib, struct
out = sys.argv[1]; R = random.Random(922)
def w(n, b): open(f"{out}/{n}.bin", "wb").write(b)
E=b"\x1b"; BEL=b"\x07"; ST=b"\x1b\\"
w("osc_titles", b"a"+E+b"]0;bel title"+BEL+b"b"+E+b"]2;st title"+ST+b"c"+E+b"]0;emb"+E+b"Xafter"+ST+b"d"+E+b"]0;t2"+E+b"[31mred"+E+b"]2;semi;co;lons"+BEL+b"tail")
w("osc_c0_inside", E+b"]0;ab\x01c\nd\x7fe\x9cf\xc3\xa9g"+BEL+b"x"+E+b"]0;x\x18y"+BEL+b"z"+E+b"]0;q\x1aw"+BEL+b"end")
w("osc8_links", E+b"]8;id=1;https://example.com/a"+BEL+b"link one"+E+b"]8;;"+BEL+b" mid "+E+b"]8;;https://ex.org/x?y=1;z"+ST+b"two\r\nwrap "*3+E+b"]8;;"+ST+b"plain"+E+b"]8;id=2:foo=bar;file:///tmp/x"+E+b"\\three"+E+b"]8;;"+E+b"\\")
w("c1_8bit", b"x\x9d0;c1 title\x9cY\x9b31mZ\xc2\x9d0;utf8c1\xc2\x9cW"+E+b"]0;in\x9cside"+BEL+b"\x90+q544e\x9c\x9f\x9eV"+E+b"P+q544e\x9c...more"+ST)
w("can_sub", E+b"]0;abort\x18me"+E+b"_Gf=32,s=1,v=1;AAAA\x18tail"+E+b"P+q54\x1a!"+E+b"^pm\x18x"+E+b"Xsos\x1ay"+E+b"]9;notify\x18no"+E+b"]9;yes notify"+BEL)
w("dcs", E+b"P+q544e;636f6c6f7273"+ST+E+b"P$qm"+ST+E+b"P$q\"p"+ST+E+b"P+q"+b"41"*700+ST+E+b"P+q"+b"4\x7f1\xc3\x01"*100+ST+E+b"Pignored\x1b[x"+ST+E+b"P1;2|data"+ST)
w("notifs", E+b"]9;hello world"+BEL+E+b"]9;4;1;50"+BEL+E+b"]777;notify;Title;Body"+ST+E+b"]99;i=1:d=0;first"+ST+E+b"]99;i=1:p=body;second"+ST+E+b"]133;A"+BEL+b"$ "+E+b"]133;B"+BEL+b"ls"+E+b"]133;C"+BEL+b"out\r\n"+E+b"]133;D;0"+BEL+E+b"]7;file://h/tmp"+BEL+E+b"]7501;?"+BEL+E+b"]6973;agent-exit;3"+BEL+E+b"]52;c;aGVsbG8="+BEL+E+b"]52;c;?"+BEL+E+b"]1337;SetMark"+BEL+E+b"]6;"+b"z"*9000+BEL+E+b"]9;after long"+BEL)
cap=2*1024*1024
def osc(body, end=BEL): return E+b"]"+body+end
w("osc_cap_minus1", osc(b"0;"+b"a"*(cap-2))+b"X"+osc(b"0;after")+b"Y")
w("osc_cap_exact",  osc(b"0;"+b"a"*(cap-1))+b"X")
w("osc_cap_plus1",  osc(b"0;"+b"a"*(cap))+b"X"+osc(b"0;recovered")+b"Y")
w("osc_cap_seps",   osc(b"0;"+b"a"*(cap-1)+b";"*5000)+b"X")
w("osc_cap_seps_over", osc(b"0;"+b"a"*(cap)+b";"*50)+b"X"+osc(b"2;rec2",ST))
w("osc_unterminated_huge", E+b"]0;"+b"b"*(cap+100)+b"\r\nline after"+ST+b"Z"+osc(b"0;fin"))
w("osc52_big", osc(b"52;c;"+base64.b64encode(bytes(R.randrange(256) for _ in range(300000)))))
w("osc_many_params", osc(b"0"+b";p"*40)+osc(b"8;"+b";"*20+b"http://x")+b"L"+osc(b"8;;"))
# kitty images
def kitty(img_bytes, chunk, s, v, ws=False, url=False, pad_each=False, i=7, extra=b""):
    if pad_each:
        parts=[img_bytes[k:k+chunk] for k in range(0,len(img_bytes),chunk)]
        chunks=[base64.b64encode(p) for p in parts]
    else:
        b=base64.b64encode(img_bytes)
        if url: b=b.replace(b"+",b"-").replace(b"/",b"_")
        if ws: b=b"".join(b[k:k+76]+b"\n" for k in range(0,len(b),76))
        chunks=[b[k:k+chunk] for k in range(0,len(b),chunk)]
    o=b""
    for idx,c in enumerate(chunks):
        m=1 if idx+1<len(chunks) else 0
        ctl=(b"a=T,f=32,s=%d,v=%d,i=%d,q=2,m=%d"%(s,v,i,m)+extra) if idx==0 else b"m=%d"%m
        o+=E+b"_G"+ctl+b";"+c+ST
    return o
px=bytes(R.randrange(256) for _ in range(64*64*4))
w("kitty_chunks_odd", b"pre"+kitty(px,4093,64,64)+b"post")
w("kitty_chunks_ws", kitty(px,4096,64,64,ws=True,i=8))
w("kitty_chunks_url", kitty(px,1000,64,64,url=True,i=9))
w("kitty_pad_each", kitty(px,3001,64,64,pad_each=True,i=10))
small=bytes(R.randrange(256) for _ in range(2*2*4))
w("kitty_small", kitty(small,3,2,2,i=11)+b"x"+kitty(small,7,2,2,i=12)+E+b"_Ga=d,d=I,i=11,q=2"+ST)
bad=bytearray(base64.b64encode(px)); bad[5000]=ord('*')
w("kitty_invalid", E+b"_Ga=T,f=32,s=64,v=64,i=13,q=2,m=1;"+bytes(bad[:8000])+ST+E+b"_Gm=0;"+bytes(bad[8000:])+ST+b"after")
w("kitty_interleaved_text", b"".join((E+b"_G"+(b"a=T,f=32,s=64,v=64,i=14,q=2," if k==0 else b"")+b"m=%d;"%(1 if k+4096<len(base64.b64encode(px)) else 0)+base64.b64encode(px)[k:k+4096]+ST+b"\x1b[32mtext\x1b[m") for k in range(0,len(base64.b64encode(px)),4096)))
def png(wd,ht):
    raw=b"".join(b"\x00"+bytes(R.randrange(256) for _ in range(wd*3)) for _ in range(ht))
    def ch(t,d): return struct.pack(">I",len(d))+t+d+struct.pack(">I",zlib.crc32(t+d)&0xffffffff)
    return b"\x89PNG\r\n\x1a\n"+ch(b"IHDR",struct.pack(">IIBBBBB",wd,ht,8,2,0,0,0))+ch(b"IDAT",zlib.compress(raw))+ch(b"IEND",b"")
p=png(50,40)
w("iterm_inline", osc(b"1337;File=name=eC5wbmc=;size=%d;inline=1:"%len(p)+base64.b64encode(p))+b"after"+osc(b"1337;File=inline=1:"+base64.b64encode(p),ST))
w("kitty_png", E+b"_Ga=T,f=100,i=20,q=2,m=1;"+base64.b64encode(p)[:777]+ST+E+b"_Gm=0;"+base64.b64encode(p)[777:]+ST)
# REP and CSI
w("rep", b"a\x1b[300b\r\n\x1b(0q\x1b[50b\x1b(B\r\n\x1b[4h\x1b[5Gx\x1b[10b\x1b[4l\r\n"+"日".encode()+b"\x1b[10b\r\n"+E+b"]8;;http://l"+BEL+b"L\x1b[60b"+E+b"]8;;"+BEL+b"\x1b[31;1mR\x1b[100b\x1b[m\x1b[38:2::1:2:3mQ\x1b[0b\x1b[65535b")
w("csi_mix", b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12;13;14;15;16;17;18;19;20;21;22;23;24;25;26;27;28;29;30;31;32;33;34mX\x1b[1\x082mY\x1b[12\x1b[mZ\x1b[?1049h\x1b[2Jalt\x1b[?1049l\x1b[?2026h\x1b[5;5Hsync\x1b[?2026l\x1b[ q\x1b[>0q\x1b[c\x1b[6n\x1b[4:3m\x1b[99999999999m\x1b[;;;m\x1b[?1000;1006h\x1b[?2004h\x1b[>1u\x1b[?u\x1b[18t\x1b[14t\x1b[16t")
# random soup
pieces=[b"txt",b"\x1b",b"\x1b]",b"\x1b]0;",b"\x1b]8;;u",b"\x1b]9;",b"\x1b]52;c;QQ==",b"\x1b_G",b"\x1b_Ga=T,f=32,s=1,v=1,i=3,m=1;",b"\x1b_Gm=0;",b"AAAA",b"QUJD",b"\x07",b"\x1b\\",b"\x18",b"\x1a",b"\x9c",b"\x9d",b"\xc2\x9c",b"\x1bP+q",b"\x1bP$q",b"41",b";",b"\x1b[",b"5",b"b",b"\x1b[3b",b"\r\n","é日".encode(),b"\x1bc",b"\x1b^",b"\x1bX",b"\x1b[?2026h",b"\x1b[?2026l",b"\x1b]133;A",b"\x1b]7501;",b" \n"]
for k in range(30):
    w(f"soup{k:02d}", b"".join(R.choice(pieces) for _ in range(R.randrange(50,400))))
