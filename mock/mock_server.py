#!/usr/bin/env python3
"""Mock scrcpy v5 server for testing rscrcpy without a phone.

Streams test.h264 in a loop with the scrcpy v5 framing (dummy byte, device meta,
codec id, session packet, config + media packets) and logs every control message
it receives as one JSON object per line in control.log.
"""
import argparse, json, os, socket, struct, sys, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
ap = argparse.ArgumentParser()
ap.add_argument("--port", type=int, default=27183)
ap.add_argument("--video", default=os.path.join(HERE, "test.h264"))
ap.add_argument("--size", default="360x640")
ap.add_argument("--log", default=os.path.join(HERE, "control.log"))
ap.add_argument("server_args", nargs="*")
args = ap.parse_args()
width, height = map(int, args.size.split("x"))

# Split the Annex B stream into access units: SPS/PPS go to the config packet.
data = open(args.video, "rb").read()
starts = []
i = 0
while (j := data.find(b"\x00\x00\x01", i)) >= 0:
    starts.append(j - 1 if j > 0 and data[j - 1] == 0 else j)
    i = j + 3
nals = [data[s:e] for s, e in zip(starts, starts[1:] + [len(data)])]
config, frames = b"", []
for n in nals:
    t = n[n.find(b"\x00\x00\x01") + 3] & 0x1F
    if t in (7, 8):
        config += n
    elif t in (1, 5):
        frames.append((n, t == 5))

log = open(args.log, "w", buffering=1)
def record(**kv):
    log.write(json.dumps(kv, ensure_ascii=False) + "\n")
record(type="server_args", args=args.server_args)

srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", args.port))
srv.listen(2)
video, _ = srv.accept()
video.sendall(b"\x00")  # dummy byte
control, _ = srv.accept()
video.sendall(b"Mock Phone".ljust(64, b"\x00"))
video.sendall(b"h264")
video.sendall(struct.pack(">III", 0x80000000, width, height))  # session packet
video.sendall(struct.pack(">QI", 1 << 62, len(config)) + config)

def recv_exact(n):
    b = b""
    while len(b) < n:
        chunk = control.recv(n - len(b))
        if not chunk:
            raise EOFError
        b += chunk
    return b

def read_control():
    try:
        while True:
            t = recv_exact(1)[0]
            if t == 0:
                action, code, repeat, meta = struct.unpack(">BiiI", recv_exact(13))
                record(type="key", action=action, keycode=code, repeat=repeat, meta=meta)
            elif t == 1:
                (n,) = struct.unpack(">I", recv_exact(4))
                record(type="text", text=recv_exact(n).decode())
            elif t == 2:
                action, pid, x, y, w, h, p, ab, b = struct.unpack(">BQiiHHHII", recv_exact(31))
                record(type="touch", action=action, pointer=pid, x=x, y=y, w=w, h=h, pressure=p)
            elif t == 3:
                x, y, w, h, hs, vs, b = struct.unpack(">iiHHhhI", recv_exact(20))
                record(type="scroll", x=x, y=y, w=w, h=h, hscroll=hs, vscroll=vs)
            elif t in (4, 8, 10):
                (v,) = recv_exact(1)
                record(type={4: "back", 8: "get_clipboard", 10: "display_power"}[t], value=v)
                if t == 8:  # answer with a CLIPBOARD device message
                    text = "mock clipboard".encode()
                    control.sendall(struct.pack(">BI", 0, len(text)) + text)
            elif t == 9:
                seq, paste, n = struct.unpack(">QBI", recv_exact(13))
                record(type="set_clipboard", paste=paste, text=recv_exact(n).decode())
            elif t in (5, 6, 7, 11, 17):
                record(type={5: "expand_notifications", 6: "expand_settings", 7: "collapse_panels",
                             11: "rotate", 17: "reset_video"}[t])
            else:
                record(type="unknown", value=t)
                return
    except (EOFError, OSError):
        pass

threading.Thread(target=read_control, daemon=True).start()

pts = 0
try:
    while True:
        for n, key in frames:
            flags = pts | ((1 << 61) if key else 0)
            video.sendall(struct.pack(">QI", flags, len(n)) + n)
            pts += 33333
            time.sleep(1 / 30)
except OSError:
    pass
sys.exit(0)
