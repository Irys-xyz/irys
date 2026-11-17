#!/usr/bin/env python3
"""Reference implementation of the prototype VDE packing (`src/vde.rs`), written for clarity.

Slow (pure Python big integers). Used to generate the known-answer values in the Rust tests:

    python3 scripts/vde_ref.py

Construction (one 256 KiB chunk = 8192 elements of 32 bytes, little-endian integers mod p):

    p       = 2^256 - 189 (prime, p = 3 mod 4)
    seed    = SHA256(address || partition_hash || chain_id LE64 || offset LE64)   (as today)
    k[i]    = SHA256(seed || LE32(i) || 0^28) mod p                              (per-element key)
    layer l = 0 .. layers-1:
        z[i] = sloth((x[i] + k[i] + l) mod p)                      every element, independent
        mix (lanes, W = 8192 / lanes; element i = lane * W + t), in order of t, then lane:
            t == 0: y[i] = z[i]
            t >= 1: prev = y[i - 1]; m = LE32(prev[0..4]) mod (t * lanes)
                    j = (m mod lanes) * W + m // lanes            (an element finished at an earlier t)
                    y[i] = (z[i] + (SHA256(prev || y[j]) mod p)) mod p
        x = y
    replica = x

sloth(x): r = x^((p+1)/4). If r^2 == x, x is a square: return the even one of {r, p - r};
          otherwise r^2 == -x: return the odd one of {r, p - r}. A permutation of [0, p).
sloth^-1(y): y even -> y^2; y odd -> p - y^2.

Decode runs the layers in reverse; the mix is undone from the stored y alone (every y[i-1], y[j]
is known), so every element decodes independently: one hash and one squaring per element per layer.
Inputs must be < p (an element >= p is not encodable in this prototype).
"""

import hashlib
import sys

P = 2**256 - 189
E = (P + 1) // 4
N = 8192


def sha(b: bytes) -> bytes:
    return hashlib.sha256(b).digest()


def le(b: bytes) -> int:
    return int.from_bytes(b, "little")


def to_le(x: int) -> bytes:
    return x.to_bytes(32, "little")


def fixed_id(offset: int):
    address = bytes(0xA0 ^ k for k in range(20))
    partition_hash = bytes((k * 7 + 3) & 0xFF for k in range(32))
    return address, partition_hash, 1270, offset


def seed_hash(cid) -> bytes:
    address, ph, chain_id, offset = cid
    return sha(address + ph + chain_id.to_bytes(8, "little") + offset.to_bytes(8, "little"))


def keys(seed: bytes):
    return [le(sha(seed + i.to_bytes(4, "little") + bytes(28))) % P for i in range(N)]


def sloth(x: int) -> int:
    r = pow(x, E, P)
    if r * r % P == x:
        return r if r % 2 == 0 else P - r
    return r if r % 2 == 1 else P - r


def sloth_inv(y: int) -> int:
    s = y * y % P
    return s if y % 2 == 0 else (P - s) % P


def mix_index(prev: int, t: int, lanes: int, w: int) -> int:
    m = (prev & 0xFFFFFFFF) % (t * lanes)
    return (m % lanes) * w + m // lanes


def encode(cid, data, layers: int, lanes: int):
    w = N // lanes
    k = keys(seed_hash(cid))
    x = list(data)
    assert all(v < P for v in x)
    for layer in range(layers):
        y = [sloth((x[i] + k[i] + layer) % P) for i in range(N)]
        for t in range(1, w):
            for lane in range(lanes):
                i = lane * w + t
                prev = y[i - 1]
                j = mix_index(prev, t, lanes, w)
                h = le(sha(to_le(prev) + to_le(y[j]))) % P
                y[i] = (y[i] + h) % P
        x = y
    return x


def decode(cid, replica, layers: int, lanes: int):
    w = N // lanes
    k = keys(seed_hash(cid))
    y = list(replica)
    for layer in reversed(range(layers)):
        z = list(y)
        for t in range(1, w):
            for lane in range(lanes):
                i = lane * w + t
                prev = y[i - 1]
                j = mix_index(prev, t, lanes, w)
                h = le(sha(to_le(prev) + to_le(y[j]))) % P
                z[i] = (y[i] - h) % P
        y = [(sloth_inv(z[i]) - k[i] - layer) % P for i in range(N)]
    return y


def digest(xs) -> str:
    return sha(b"".join(to_le(v) for v in xs)).hex()


def main():
    cid = fixed_id(7)
    zeros = [0] * N
    for lanes in (1, 64, 8192):
        out = encode(cid, zeros, 2, lanes)
        assert decode(cid, out, 2, lanes) == zeros
        print(f"layers=2 lanes={lanes:5} sha256(replica)={digest(out)} elem0={to_le(out[0]).hex()}")
    data = [le(sha(b"data" + i.to_bytes(4, "little"))) % P for i in range(N)]
    out = encode(cid, data, 1, 64)
    assert decode(cid, out, 1, 64) == data
    print(f"layers=1 lanes=   64 data sha256(replica)={digest(out)}")
    # sloth sanity
    for x in (0, 1, 2, 3, P - 1, P - 2):
        assert sloth_inv(sloth(x)) == x
    print("ok", file=sys.stderr)


if __name__ == "__main__":
    main()
