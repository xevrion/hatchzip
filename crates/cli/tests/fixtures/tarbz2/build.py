"""Rebuild small fixtures with GNU tar and bzip2, including deliberate decoys."""
import bz2
import os
from pathlib import Path
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
TAR = os.environ.get('TAR_TOOL', 'tar')
BZIP2 = os.environ.get('BZIP2_TOOL', 'bzip2')
MAGIC = f'{0x314159265359:048b}'
END = f'{0x177245385090:048b}'


def bits(data):
    return ''.join(f'{b:08b}' for b in data)


def packed(s):
    s += '0' * (-len(s) % 8)
    return int(s, 2).to_bytes(len(s) // 8, 'big')


def compress(data, level=9):
    return subprocess.run([BZIP2, f'-{level}', '-c'], input=data,
                          stdout=subprocess.PIPE, check=True).stdout


def block(data):
    s = bits(compress(data))
    end = s.index(END, 112)
    assert s[32:80] == MAGIC
    assert MAGIC not in s[80:end]
    return s[32:end]


def stream(blocks):
    crc = 0
    for b in blocks:
        crc = ((crc << 1) | (crc >> 31)) & 0xffffffff
        crc ^= int(b[48:80], 2)
    return packed(bits(b'BZh9') + ''.join(blocks) + END + f'{crc:032b}')


def noise(n, seed):
    x = (seed * 0x9E3779B97F4A7C15) & ((1 << 64) - 1) | 1
    out = bytearray()
    for _ in range(n):
        x ^= (x << 13) & ((1 << 64) - 1)
        x ^= x >> 7
        x ^= (x << 17) & ((1 << 64) - 1)
        out.append(x & 255)
    return bytes(out)


def tar(root, files):
    (root / 'game/saves').mkdir(parents=True, exist_ok=True)
    for name, data in files.items():
        p = root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_bytes(data)
    return subprocess.run([TAR, '--format=gnu', '--mtime=@1700000000',
                           '--owner=0', '--group=0', '--numeric-owner',
                           '-cf', '-', *sorted(files), 'game/saves'], cwd=root,
                          stdout=subprocess.PIPE, check=True).stdout


def decoy_table(alpha, start):
    # An unused table can contain arbitrary valid length deltas. Pairs of
    # opposite deltas place the signature without changing any used code.
    delta = 465 - start - 5
    assert delta >= 0, start
    prefix = '1011' * (delta // 4) + '0' * (delta % 4)
    encoded = f'{10:05b}' + prefix + MAGIC
    pos, curr, count = 5, 10, 0
    while pos < len(encoded):
        while pos < len(encoded) and encoded[pos] == '1':
            if pos + 1 == len(encoded):
                encoded += '0'
            curr += 1 if encoded[pos + 1] == '0' else -1
            assert 1 <= curr <= 20
            pos += 2
        if pos == len(encoded):
            encoded += '0'
        pos += 1
        count += 1
    assert count <= alpha, (count, alpha)
    return encoded + '0' * (alpha - count)


def false_table(data):
    b = block(data)
    pos = 105
    groups = b[pos:pos + 16]
    pos += 16
    used = 0
    for flag in groups:
        if flag == '1':
            used += b[pos:pos + 16].count('1')
            pos += 16
    alpha = used + 2
    group_at = pos
    ngroups = int(b[pos:pos + 3], 2)
    pos += 3
    nselectors = int(b[pos:pos + 15], 2)
    pos += 15
    mtf = list(range(ngroups))
    selectors = []
    for _ in range(nselectors):
        n = 0
        while b[pos] == '1':
            n += 1
            pos += 1
        pos += 1
        selectors.append(mtf.pop(n))
        mtf.insert(0, selectors[-1])
    table_at = pos
    # Insert table zero and shift used table numbers up by one.
    mtf = list(range(ngroups + 1))
    encoded_selectors = ''
    for old in selectors:
        n = mtf.index(old + 1)
        encoded_selectors += '1' * n + '0'
        mtf.insert(0, mtf.pop(n))
    head = b[:group_at] + f'{ngroups + 1:03b}{nselectors:015b}' + encoded_selectors
    table = decoy_table(alpha, 32 + len(head))
    result = stream([head + table + b[table_at:]])
    assert bz2.decompress(result) == data
    assert offsets(result) == [32, 465], offsets(result)
    return result


def crc(data):
    value = 0xffffffff
    for byte in data:
        value ^= byte << 24
        for _ in range(8):
            value = ((value << 1) ^ (0x04c11db7 if value & 0x80000000 else 0)) & 0xffffffff
    return value ^ 0xffffffff


def early_block():
    # Put the decoy one bit into inUse16, at block bit 106. The remaining
    # bitmap advertises unused symbols as well as the byte actually coded.
    mapping = '1' + MAGIC
    groups = mapping[:16]
    flags = mapping[16:].ljust(groups.count('1') * 16, '0')
    alphabet = []
    pos = 0
    for group, active in enumerate(groups):
        if active == '1':
            for i, flag in enumerate(flags[pos:pos + 16]):
                if flag == '1':
                    alphabet.append(16 * group + i)
            pos += 16
    data = bytes([alphabet[0]]) * 3
    alpha = len(alphabet) + 2
    width = (alpha - 1).bit_length()
    table = f'{width:05b}' + '0' * alpha
    # Three MTF zeros are RUNA, RUNA, then end-of-block.
    payload = ''.join(f'{token:0{width}b}' for token in [0, 0, alpha - 1])
    b = (MAGIC + f'{crc(data):032b}' + '0' + '0' * 24 + groups + flags
         + '010' + f'{1:015b}' + '0' + table * 2 + payload)
    assert b[106:154] == MAGIC
    assert bz2.decompress(stream([b])) == data
    return data, b


def offsets(data):
    s = bits(data)
    return [i for i in range(len(s) - 47) if s.startswith(MAGIC, i)]


def main():
    subprocess.run([TAR, '--version'], check=True)
    subprocess.run([BZIP2, '--help'], check=True)
    with tempfile.TemporaryDirectory(dir=HERE) as tmp:
        root = Path(tmp)
        files = {
            'game/data/noise.bin': noise(4096, 34) * 400,
            'game/data/text.txt': b'every durable point keeps the archive safe\n' * 50000,
            'game/empty.txt': b'',
            'game/' + 'long-folder-' * 10 + '/deep.txt': b'long name\n',
        }
        data = tar(root, files)
        for level in [1, 9]:
            (HERE / f'level{level}.tar.bz2').write_bytes(compress(data, level))
        cuts = [0, 777, 1500777, len(data)]
        multi = compress(b'') + b''.join(compress(data[a:b], 1 if i % 2 else 9)
                                        for i, (a, b) in enumerate(zip(cuts, cuts[1:])))
        (HERE / 'multi.tar.bz2').write_bytes(multi)
        corrupt = bytearray((HERE / 'level1.tar.bz2').read_bytes())
        corrupt[100] ^= 1
        (HERE / 'corrupt-block.tar.bz2').write_bytes(corrupt)
        s = bits((HERE / 'level1.tar.bz2').read_bytes())
        trailer = s.rindex(END)
        s = s[:trailer + 48] + ('1' if s[trailer + 48] == '0' else '0') + s[trailer + 49:]
        (HERE / 'bad-stream-crc.tar.bz2').write_bytes(packed(s))
        (HERE / 'not-tar.bz2').write_bytes(compress(b'one plain file\n'))
        empty = tar(root, {'game/empty.txt': b''})
        (HERE / 'false-magic.tar.bz2').write_bytes(false_table(empty))
        content, second = early_block()
        early = tar(root, {'game/early.bin': content})
        first, last = block(early[:512]), block(early[515:])
        result = stream([first, second, last])
        assert bz2.decompress(result) == early
        real_second = 32 + len(first)
        assert offsets(result) == [32, real_second, real_second + 106, real_second + len(second)]
        (HERE / 'early-false-magic.tar.bz2').write_bytes(result)
        print('early fixture block and decoy positions:', offsets(result))
        print('early fixture content:', content.hex())
        for p in HERE.glob('*.bz2'):
            if p.name not in ['corrupt-block.tar.bz2', 'bad-stream-crc.tar.bz2']:
                decoded = subprocess.run([BZIP2, '-dc'], input=p.read_bytes(),
                                         stdout=subprocess.PIPE, check=True).stdout
                assert decoded == bz2.decompress(p.read_bytes())
            print(p.name, p.stat().st_size)


if __name__ == '__main__':
    main()
