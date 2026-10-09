import re, sys, os
os.chdir(sys.argv[1])
names = ["baseline", "block-16k", "block-256k", "cache-1g", "blobs", "no-compress", "direct-io", "universal"]

def shape(t, s):
    m = re.search(r"shape=" + s + r" .*?p50_us=(\d+) p99_us=(\d+).*?disk_reads=(\d+).*?disk_writes=(\d+).*?per_lookup_read_bytes=(\d+)", t)
    return tuple(int(x) for x in m.groups()) if m else (0, 0, 0, 0, 0)

def parse(path, engine="rocks"):
    txt = open(path).read()
    i = txt.find("engine=rocks")
    t = txt[i:] if engine == "rocks" else txt[:i]
    d = {}
    m = re.search(r"index_ms=(\d+) data_path_ms=(\d+) settle_ms=(\d+)", t)
    d["w"] = tuple(int(x) / 1000 for x in m.groups()) if m else (0, 0, 0)
    m = re.search(r"logical_bytes=(\d+)", t)
    d["sz"] = int(m.group(1)) / 1e9 if m else 0
    for s in ["data_path_random", "serve", "range", "rmw_path", "data_path_seq"]:
        d[s] = shape(t, s)
    m = re.search(r"mem engine=" + engine + r" phase=peak rss_kb=(\d+) anon_kb=(\d+)", t)
    d["anon"] = int(m.group(2)) / 1024 if m else 0
    return d

hdr = "| preset | write idx+dp+settle (s) | on disk GB | random p50/p99 ms | random reads/lookup | random disk_writes | serve p50/p99 ms | range p50 ms | rmw_path p50 ms | seq p50 ms | peak anon MB |"
print(hdr)
print("|" + "---|" * 11)
rows = [("MDBX control (8 tx/commit)", parse("mid-baseline.txt", "mdbx"))]
for n in names:
    rows.append((n, parse("mid-baseline.txt" if n == "baseline" else f"mid-{n}.txt")))
for n, d in rows:
    w, r, s, g, rm, q = d["w"], d["data_path_random"], d["serve"], d["range"], d["rmw_path"], d["data_path_seq"]
    print(f"| {n} | {w[0]:.0f}+{w[1]:.0f}+{w[2]:.1f} = {sum(w):.0f} | {d['sz']:.2f} | {r[0]/1000:.1f} / {r[1]/1000:.1f} | {r[2]/1024:.2f} | {r[3]} | {s[0]/1000:.1f} / {s[1]/1000:.1f} | {g[0]/1000:.2f} | {rm[0]/1000:.1f} | {q[0]/1000:.2f} | {d['anon']:.0f} |")
