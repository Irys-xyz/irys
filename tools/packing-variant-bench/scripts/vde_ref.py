#!/usr/bin/env python3
"""Reference implementation of the prototype VDE packing (`src/vde.rs`), written for clarity.

Slow (pure Python big integers). Used to generate the known-answer values in the Rust tests:

    python3 scripts/vde_ref.py

Construction (one 256 KiB chunk = 8192 elements of 32 bytes, little-endian integers mod p):

    p       = 2^256 - 189 (prime, p = 3 mod 4); M = 2^256 (all other arithmetic is mod M)
    seed    = SHA256(address || partition_hash || chain_id LE64 || offset LE64)   (as today)
    k[i]    = SHA256(seed || LE32(i) || 0^28)                                    (per-element key)
    layer l = 0 .. layers-1:
        roots (chains, Wc = 8192 / chains; element i = chain * Wc + t):
            z[i] = root((x[i] + k[i] + l + (z[i - 1] if t > 0 else 0)) mod M)
        chains = 8192: every root independent. chains = 1: all 8192 roots in series, so a
        layer's encode latency is 8192 sequential roots however much hardware is used.
        mix (lanes, W = 8192 / lanes; element i = lane * W + t), in order of t, then lane:
            t == 0: y[i] = z[i]
            t >= 1: prev = y[i - 1]; m = LE32(prev[0..4]) mod (t * lanes)
                    j = (m mod lanes) * W + m // lanes            (an element finished at an earlier t)
                    y[i] = (z[i] + SHA256(prev || y[j])) mod M
        x = y
    replica = x

root(x): sloth(x) for x < p; the 189 values p..M-1 pass through unchanged. A permutation of
         [0, M), so any 32-byte input encodes and chunks keep their size. Crafted data can make
         layer 0 skip its roots; the layer-0 mix then adds hashes it cannot steer.
sloth(x): r = x^((p+1)/4). If r^2 == x, x is a square: return the even one of {r, p - r};
          otherwise r^2 == -x: return the odd one of {r, p - r}. A permutation of [0, p).
sloth^-1(y): y even -> y^2; y odd -> p - y^2.

Decode runs the layers in reverse; the mix is undone from the stored y alone (every y[i-1], y[j]
is known), then the roots from the recovered z alone (every z[i-1] is known), so every element
decodes independently: one hash and one squaring per element per layer.
"""

import hashlib
import sys

P = 2**256 - 189
E = (P + 1) // 4
N = 8192
M = 2**256


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
    return [le(sha(seed + i.to_bytes(4, "little") + bytes(28))) for i in range(N)]


def sloth(x: int) -> int:
    r = pow(x, E, P)
    if r * r % P == x:
        return r if r % 2 == 0 else P - r
    return r if r % 2 == 1 else P - r


def sloth_inv(y: int) -> int:
    s = y * y % P
    return s if y % 2 == 0 else (P - s) % P


def root(x: int) -> int:
    return sloth(x) if x < P else x


def root_inv(y: int) -> int:
    return sloth_inv(y) if y < P else y


def mix_index(prev: int, t: int, lanes: int, w: int) -> int:
    m = (prev & 0xFFFFFFFF) % (t * lanes)
    return (m % lanes) * w + m // lanes


def encode(cid, data, layers: int, lanes: int, chains: int = N):
    w = N // lanes
    k = keys(seed_hash(cid))
    x = list(data)
    for layer in range(layers):
        wc = N // chains
        y = [0] * N
        for i in range(N):
            chained = y[i - 1] if i % wc else 0
            y[i] = root((x[i] + k[i] + layer + chained) % M)
        for t in range(1, w):
            for lane in range(lanes):
                i = lane * w + t
                prev = y[i - 1]
                j = mix_index(prev, t, lanes, w)
                h = le(sha(to_le(prev) + to_le(y[j])))
                y[i] = (y[i] + h) % M
        x = y
    return x


def decode(cid, replica, layers: int, lanes: int, chains: int = N):
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
                h = le(sha(to_le(prev) + to_le(y[j])))
                z[i] = (y[i] - h) % M
        wc = N // chains
        y = [(root_inv(z[i]) - k[i] - layer - (z[i - 1] if i % wc else 0)) % M for i in range(N)]
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
    data = [le(sha(b"data" + i.to_bytes(4, "little"))) for i in range(N)]
    out = encode(cid, data, 1, 64)
    assert decode(cid, out, 1, 64) == data
    print(f"layers=1 lanes=   64 data sha256(replica)={digest(out)}")
    # crafted data: every element lands at or above p after the layer-0 key, so layer 0 roots pass through
    k = keys(seed_hash(cid))
    crafted = [(P + (i % 189) - k[i]) % M for i in range(N)]
    out = encode(cid, crafted, 2, 64)
    assert decode(cid, out, 2, 64) == crafted
    print(f"layers=2 lanes=   64 crafted sha256(replica)={digest(out)}")
    for chains in (1, 64):
        out = encode(cid, data, 2, 64, chains)
        assert decode(cid, out, 2, 64, chains) == data
        print(f"layers=2 lanes=   64 chains={chains} data sha256(replica)={digest(out)}")
    # sloth sanity
    for x in (0, 1, 2, 3, P - 1, P - 2):
        assert sloth_inv(sloth(x)) == x
    print("ok", file=sys.stderr)


if __name__ == "__main__":
    main()
