#!/usr/bin/env python3
"""Checks a Realtek RTL87x2G app image the way the keyboard's updater does:
image id, model string, SHA-256 over the hashed range, size against the
staging bank. Usage: check_image.py <image.bin> [MODEL] (MODEL e.g. KCZKV38K)."""
import hashlib, struct, sys

if len(sys.argv) < 2:
    sys.exit(__doc__)
path = sys.argv[1]
expected_model = sys.argv[2].encode() if len(sys.argv) > 2 else None
d = open(path, "rb").read()
HEADER = 0x500
OTA_TMP = 356 * 1024
img_hash = d[0x180:0x1A0]
_crc16, ic_type, secure_ver, ctrl_flag, image_id, payload_len = struct.unpack_from("<HBBHHI", d, 0x1A0)
model = d[0x208:0x210]
sha = hashlib.sha256(d[0x1A0:]).digest()
signature_blank = all(b in (0x00, 0xFF) for b in d[0x10:0x180])
print(f"{path}: {len(d)} bytes")
print(f"  image_id=0x{image_id:04X} (app image is 0x37A9)  ic_type=0x{ic_type:X}  secure_version={secure_ver}  enc={(ctrl_flag >> 1) & 1}")
print(f"  payload_len={payload_len} (file minus header = {len(d) - HEADER})  model={model!r}")
print(f"  sha256 matches header: {sha == img_hash}   signature area blank: {signature_blank}")
print(f"  fits the staging bank ({OTA_TMP} bytes): {len(d) <= OTA_TMP}")
ok = image_id == 0x37A9 and sha == img_hash and payload_len == len(d) - HEADER and len(d) <= OTA_TMP
if expected_model is not None:
    ok = ok and model == expected_model
    print(f"  model is {expected_model!r}: {model == expected_model}")
print("  VERDICT:", "OK" if ok else "MISMATCH")
sys.exit(0 if ok else 1)
