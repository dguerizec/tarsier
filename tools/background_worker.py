"""Generic GLSL background producer. stdin: package JSON; stdout: one IPC handshake.

TARSFRM1 uses the same bounded BGR frame format as perception IPC. The memfd
lifetime is owned by this process; flock protects a complete publication.
"""
import ctypes
import fcntl
import json
import mmap
import os
import struct
import sys
import time

import moderngl


def run():
    package = json.loads(sys.stdin.readline())
    width, height, fps = package["width"], package["height"], package["fps"]
    if not (16 <= width <= 1280 and 16 <= height <= 720 and 1 <= fps <= 30):
        raise ValueError("unsupported render dimensions or cadence")
    ctx = moderngl.create_standalone_context(require=330, backend="egl")
    program = ctx.program(
        vertex_shader="""#version 330
        in vec2 position;
        void main() { gl_Position = vec4(position, 0.0, 1.0); }
        """,
        fragment_shader=package["source"],
    )
    vertices = ctx.buffer(struct.pack("6f", -1, -1, 3, -1, -1, 3))
    vao = ctx.simple_vertex_array(program, vertices, "position")
    target = ctx.simple_framebuffer((width, height), components=3)
    target.use()
    if "resolution" in program:
        program["resolution"].value = (width, height)
    params = package["parameters"]
    for name, value in params.items():
        if name != "speed" and name in program:
            program[name].value = value
    stride = (width * 3 + 3) & ~3
    length = 64 + stride * height
    # Standalone Python builds do not always expose Linux memfd/seal constants.
    libc = ctypes.CDLL(None, use_errno=True)
    libc.memfd_create.argtypes = [ctypes.c_char_p, ctypes.c_uint]
    libc.memfd_create.restype = ctypes.c_int
    fd = libc.memfd_create(b"tarsier-background", 0x0001 | 0x0002)
    if fd < 0:
        raise OSError(ctypes.get_errno(), "memfd_create")
    os.fchmod(fd, 0o600)
    os.ftruncate(fd, length)
    fcntl.fcntl(fd, 1033, 0x0002 | 0x0004 | 0x0001)  # F_ADD_SEALS: shrink/grow/seal
    shared = mmap.mmap(fd, length)
    print(json.dumps({"pid": os.getpid(), "fd": fd, "renderer": ctx.info["GL_RENDERER"]}), flush=True)
    pixels = bytearray(stride * height)
    sequence = 0
    start = time.monotonic()
    parent = os.getppid()
    while os.getppid() == parent:
        tick = time.monotonic()
        if "time" in program:
            program["time"].value = (tick - start) * params.get("speed", 1.0)
        vao.render(moderngl.TRIANGLES)
        raw = target.read(components=3, alignment=1)
        # OpenGL's bottom-up RGB becomes top-down, padded BGR.
        for y in range(height):
            row = raw[(height - 1 - y) * width * 3:(height - y) * width * 3]
            offset = y * stride
            pixels[offset:offset + width * 3:3] = row[2::3]
            pixels[offset + 1:offset + width * 3:3] = row[1::3]
            pixels[offset + 2:offset + width * 3:3] = row[0::3]
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            try:
                sequence += 1
                header = struct.pack("<8sIIIIQQQQ8x", b"TARSFRM1", width, height,
                                     stride, 0, sequence, sequence,
                                     time.time_ns() // 1_000_000, len(pixels))
                shared[64:] = pixels
                shared[:64] = header
            finally:
                fcntl.flock(fd, fcntl.LOCK_UN)
        time.sleep(max(0, 1 / fps - (time.monotonic() - tick)))


if __name__ == "__main__":
    run()
