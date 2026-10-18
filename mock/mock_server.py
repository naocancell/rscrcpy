# Mock scrcpy v5 server for testing rscrcpy without a phone.
import socket, struct, time, sys
data = open(sys.argv[1] if len(sys.argv) > 1 else "test.h264", "rb").read()
# split Annex B into NAL units
idx = []
i = 0
while True:
    j = data.find(b"\x00\x00\x01", i)
    if j < 0: break
    start = j - 1 if j > 0 and data[j-1] == 0 else j
    idx.append((start, j + 3)); i = j + 3
nals = []
for k, (s, p) in enumerate(idx):
    e = idx[k+1][0] if k + 1 < len(idx) else len(data)
    nals.append(data[s:e])
config, frames = b"", []
for n in nals:
    t = n[n.find(b"\x00\x00\x01") + 3] & 0x1f
    if t in (7, 8): config += n
    elif t in (1, 5): frames.append((n, t == 5))
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 27183)); srv.listen(2)
video, _ = srv.accept(); video.sendall(b"\x00")
control, _ = srv.accept()
log = open("control.log", "wb")
control.setblocking(False)
video.sendall(b"Mock Phone".ljust(64, b"\x00"))
video.sendall(b"h264")
video.sendall(struct.pack(">III", 0x80000000, 540, 960))
video.sendall(struct.pack(">QI", 1 << 62, len(config)) + config)
pts = 0
for n, key in frames * 3:
    flags = pts | ((1 << 61) if key else 0)
    try:
        video.sendall(struct.pack(">QI", flags, len(n)) + n)
    except OSError:
        break
    try:
        d = control.recv(4096)
        if d: log.write(d); log.flush()
    except BlockingIOError:
        pass
    pts += 33333; time.sleep(1/30)
