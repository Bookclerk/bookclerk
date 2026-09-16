"""Unpacked single-segment Cap'n Proto builders and readers.

Private runtime under the generated ``_wire`` codecs and the hand-written
``db_value`` helpers. Encodes the ``write_message`` single-segment stream
format produced by ``capnpc-rust``; far pointers and multi-segment messages
are rejected. Reads follow Cap'n Proto evolution semantics: fields beyond the
actual struct size decode as their zero default, and a null struct pointer
decodes as an all-default struct.
"""

from __future__ import annotations

import struct
from typing import Any, Protocol

WORD = 8
_MAX_TRAVERSAL_WORDS = 64 * 1024
_U64_MASK = (1 << 64) - 1


class _CapTable(Protocol):
    """Transport capability table for interface-typed fields.

    A codec never sees the transport; it hands capability values to the table
    and stores the returned index in the message (and vice versa on read).
    """

    def export_cap(self, value: Any) -> int:
        """Register ``value`` and return its capability index."""
        ...

    def import_cap(self, index: int | None) -> Any:
        """Resolve a capability index (``None`` for a null pointer)."""
        ...


class _NoCaps:
    """Capability table for messages that carry no interface fields."""

    def export_cap(self, value: Any) -> int:
        raise ValueError("message carries a capability but no CapTable was provided")

    def import_cap(self, index: int | None) -> Any:
        if index is None:
            return None
        raise ValueError("message carries a capability but no CapTable was provided")


NO_CAPS: _CapTable = _NoCaps()


class _CapnpMessage:
    """Single-segment unpacked Cap'n builder."""

    def __init__(self) -> None:
        self._buf = bytearray(256)
        self.used_words = 1

    def alloc(self, n_words: int) -> int:
        off = self.used_words
        self.used_words += n_words
        need = (self.used_words + 1) * WORD
        if len(self._buf) < need:
            self._buf.extend(b"\x00" * (need - len(self._buf)))
        return off

    def init_root(self, data_words: int, pointer_words: int) -> _CapnpStruct:
        off = self.alloc(data_words + pointer_words)
        self.write_struct_pointer(0, off, data_words, pointer_words)
        return _CapnpStruct(self, off, data_words, pointer_words)

    def finish(self) -> bytes:
        seg = bytes(self._buf[: self.used_words * WORD])
        header = struct.pack("<II", 0, self.used_words)
        return header + seg

    def write_struct_pointer(
        self, ptr_word: int, target_word: int, data_words: int, pointer_words: int
    ) -> None:
        offset = target_word - (ptr_word + 1)
        word = 0 | ((offset & 0x3FFFFFFF) << 2) | (data_words << 32) | (pointer_words << 48)
        self.set_word(ptr_word, word)

    def write_list_pointer(
        self, ptr_word: int, target_word: int, element_size: int, list_length: int
    ) -> None:
        offset = target_word - (ptr_word + 1)
        word = 1 | ((offset & 0x3FFFFFFF) << 2) | (element_size << 32) | (list_length << 35)
        self.set_word(ptr_word, word)

    def write_cap_pointer(self, ptr_word: int, index: int) -> None:
        self.set_word(ptr_word, 3 | (index << 32))

    def init_struct_list(
        self, ptr_word: int, count: int, data_words: int, pointer_words: int
    ) -> list[_CapnpStruct]:
        if count == 0:
            tag_word = self.alloc(1)
            self.write_list_pointer(ptr_word, tag_word, 7, 0)
            tag = 0 | (data_words << 32) | (pointer_words << 48)
            self.set_word(tag_word, tag)
            return []
        elem_words = data_words + pointer_words
        payload_words = count * elem_words
        tag_word = self.alloc(1 + payload_words)
        self.write_list_pointer(ptr_word, tag_word, 7, payload_words)
        tag = 0 | (count << 2) | (data_words << 32) | (pointer_words << 48)
        self.set_word(tag_word, tag)
        return [
            _CapnpStruct(self, tag_word + 1 + i * elem_words, data_words, pointer_words)
            for i in range(count)
        ]

    def set_text(self, ptr_word: int, value: str) -> None:
        self.set_byte_list(ptr_word, value.encode("utf-8") + b"\x00")

    def set_data(self, ptr_word: int, value: bytes) -> None:
        self.set_byte_list(ptr_word, bytes(value))

    def set_byte_list(self, ptr_word: int, data: bytes) -> None:
        if not data:
            self.write_list_pointer(ptr_word, ptr_word + 1, 2, 0)
            return
        n_words = (len(data) + WORD - 1) // WORD
        target = self.alloc(n_words)
        start = target * WORD
        self._buf[start : start + len(data)] = data
        self.write_list_pointer(ptr_word, target, 2, len(data))

    def set_pointer_list(self, ptr_word: int, count: int) -> list[int]:
        """Allocate a list of ``count`` pointers; returns each element's word."""
        if count == 0:
            self.write_list_pointer(ptr_word, ptr_word + 1, 6, 0)
            return []
        target = self.alloc(count)
        self.write_list_pointer(ptr_word, target, 6, count)
        return [target + i for i in range(count)]

    def set_text_list(self, ptr_word: int, values: list[str]) -> None:
        for word, value in zip(self.set_pointer_list(ptr_word, len(values)), values, strict=True):
            self.set_text(word, value)

    def set_data_list(self, ptr_word: int, values: list[bytes]) -> None:
        for word, value in zip(self.set_pointer_list(ptr_word, len(values)), values, strict=True):
            self.set_data(word, value)

    def set_uint_list(self, ptr_word: int, values: list[int], byte_width: int) -> None:
        """Write a ``List(UInt16)`` (``byte_width=2``) or ``List(UInt32)`` (``4``)."""
        count = len(values)
        size_code = 3 if byte_width == 2 else 4
        if count == 0:
            self.write_list_pointer(ptr_word, ptr_word + 1, size_code, 0)
            return
        n_words = (count * byte_width + WORD - 1) // WORD
        target = self.alloc(n_words)
        base = target * WORD
        for i, value in enumerate(values):
            self._buf[base + i * byte_width : base + (i + 1) * byte_width] = int(value).to_bytes(
                byte_width, "little", signed=False
            )
        self.write_list_pointer(ptr_word, target, size_code, count)

    def set_bool_list(self, ptr_word: int, values: list[bool]) -> None:
        count = len(values)
        if count == 0:
            self.write_list_pointer(ptr_word, ptr_word + 1, 1, 0)
            return
        n_words = (count + 63) // 64
        target = self.alloc(n_words)
        base = target * WORD
        for i, value in enumerate(values):
            if value:
                self._buf[base + (i >> 3)] |= 1 << (i & 7)
        self.write_list_pointer(ptr_word, target, 1, count)

    def set_u16(self, word: int, field_index: int, value: int) -> None:
        struct.pack_into("<H", self._buf, word * WORD + field_index * 2, value & 0xFFFF)

    def set_i32(self, word: int, field_index: int, value: int) -> None:
        struct.pack_into("<i", self._buf, word * WORD + field_index * 4, value)

    def set_u32(self, word: int, field_index: int, value: int) -> None:
        struct.pack_into("<I", self._buf, word * WORD + field_index * 4, value & 0xFFFFFFFF)

    def set_i64(self, word: int, field_index: int, value: int) -> None:
        struct.pack_into("<q", self._buf, word * WORD + field_index * 8, value)

    def set_u64(self, word: int, field_index: int, value: int) -> None:
        struct.pack_into("<Q", self._buf, word * WORD + field_index * 8, value & _U64_MASK)

    def set_f32(self, word: int, field_index: int, value: float) -> None:
        struct.pack_into("<f", self._buf, word * WORD + field_index * 4, value)

    def set_f64(self, word: int, field_index: int, value: float) -> None:
        struct.pack_into("<d", self._buf, word * WORD + field_index * 8, value)

    def set_bool(self, word: int, bit_index: int, value: bool) -> None:
        byte_off = word * WORD + (bit_index >> 3)
        mask = 1 << (bit_index & 7)
        if value:
            self._buf[byte_off] |= mask
        else:
            self._buf[byte_off] &= ~mask

    def set_word(self, word: int, value: int) -> None:
        struct.pack_into("<Q", self._buf, word * WORD, value & _U64_MASK)


class _CapnpStruct:
    """Struct cursor on a :class:`_CapnpMessage`."""

    def __init__(
        self, msg: _CapnpMessage, word: int, data_words: int, pointer_words: int
    ) -> None:
        self.msg = msg
        self.word = word
        self.data_words = data_words
        self.pointer_words = pointer_words

    def pointer_word(self, index: int) -> int:
        if index >= self.pointer_words:
            raise ValueError("pointer index outside struct pointer section")
        return self.word + self.data_words + index

    def set_u16(self, field_index: int, value: int) -> None:
        self.msg.set_u16(self.word, field_index, value)

    def set_i32(self, field_index: int, value: int) -> None:
        self.msg.set_i32(self.word, field_index, value)

    def set_u32(self, field_index: int, value: int) -> None:
        self.msg.set_u32(self.word, field_index, value)

    def set_i64(self, field_index: int, value: int) -> None:
        self.msg.set_i64(self.word, field_index, value)

    def set_u64(self, field_index: int, value: int) -> None:
        self.msg.set_u64(self.word, field_index, value)

    def set_f32(self, field_index: int, value: float) -> None:
        self.msg.set_f32(self.word, field_index, value)

    def set_f64(self, field_index: int, value: float) -> None:
        self.msg.set_f64(self.word, field_index, value)

    def set_bool(self, bit_index: int, value: bool) -> None:
        self.msg.set_bool(self.word, bit_index, value)

    def set_text(self, pointer_index: int, value: str) -> None:
        self.msg.set_text(self.pointer_word(pointer_index), value)

    def set_data(self, pointer_index: int, value: bytes) -> None:
        self.msg.set_data(self.pointer_word(pointer_index), value)

    def set_cap(self, pointer_index: int, index: int) -> None:
        self.msg.write_cap_pointer(self.pointer_word(pointer_index), index)

    def set_text_list(self, pointer_index: int, values: list[str]) -> None:
        self.msg.set_text_list(self.pointer_word(pointer_index), values)

    def set_data_list(self, pointer_index: int, values: list[bytes]) -> None:
        self.msg.set_data_list(self.pointer_word(pointer_index), values)

    def set_bool_list(self, pointer_index: int, values: list[bool]) -> None:
        self.msg.set_bool_list(self.pointer_word(pointer_index), values)

    def set_u16_list(self, pointer_index: int, values: list[int]) -> None:
        self.msg.set_uint_list(self.pointer_word(pointer_index), values, 2)

    def set_u32_list(self, pointer_index: int, values: list[int]) -> None:
        self.msg.set_uint_list(self.pointer_word(pointer_index), values, 4)

    def init_struct_list(
        self, pointer_index: int, count: int, data_words: int, pointer_words: int
    ) -> list[_CapnpStruct]:
        return self.msg.init_struct_list(
            self.pointer_word(pointer_index), count, data_words, pointer_words
        )

    def init_struct(
        self, pointer_index: int, data_words: int, pointer_words: int
    ) -> _CapnpStruct:
        ptr = self.pointer_word(pointer_index)
        off = self.msg.alloc(data_words + pointer_words)
        self.msg.write_struct_pointer(ptr, off, data_words, pointer_words)
        return _CapnpStruct(self.msg, off, data_words, pointer_words)


def _sign_extend_30(n: int) -> int:
    n &= 0x3FFFFFFF
    if n & 0x20000000:
        return n - 0x40000000
    return n


class _ListPointer:
    __slots__ = ("target", "element_size", "length")

    def __init__(self, target: int, element_size: int, length: int) -> None:
        self.target = target
        self.element_size = element_size
        self.length = length


class _CapnpReader:
    """Single-segment unpacked Cap'n reader."""

    def __init__(self, data: bytes) -> None:
        if len(data) < WORD:
            raise ValueError("truncated Cap'n message")
        nseg_minus, size0 = struct.unpack_from("<II", data, 0)
        if nseg_minus + 1 != 1:
            raise ValueError("multi-segment Cap'n messages are not supported")
        self._data = data
        self._seg = WORD
        self._size0 = size0
        if self._seg + size0 * WORD > len(data):
            raise ValueError("truncated Cap'n segment")
        if size0 > _MAX_TRAVERSAL_WORDS:
            raise ValueError("Cap'n segment exceeds traversal budget")

    def root(self, data_words: int, pointer_words: int) -> _StructReader:
        return self.struct_at(0, data_words, pointer_words)

    def struct_at(self, ptr_word: int, data_words: int, pointer_words: int) -> _StructReader:
        word = self.read_word(ptr_word)
        if word == 0:
            return _StructReader(self, 0, 0, 0)
        a = word & 3
        if a == 2 or a == 3:
            raise ValueError("far pointers are not supported")
        if a != 0:
            raise ValueError("expected struct pointer")
        offset = _sign_extend_30((word >> 2) & 0x3FFFFFFF)
        dw = (word >> 32) & 0xFFFF
        pw = (word >> 48) & 0xFFFF
        target = ptr_word + 1 + offset
        self._check_range(target, dw + pw)
        return _StructReader(self, target, dw, pw)

    def read_word(self, word: int) -> int:
        self._check_range(word, 1)
        return struct.unpack_from("<Q", self._data, self._seg + word * WORD)[0]

    def _check_range(self, word: int, n_words: int) -> None:
        if word < 0 or n_words < 0 or word + n_words > self._size0:
            raise ValueError("Cap'n pointer out of segment")

    def _list_pointer(self, ptr_word: int, expect_size: int) -> _ListPointer | None:
        word = self.read_word(ptr_word)
        if word == 0:
            return None
        a = word & 3
        if a == 2 or a == 3:
            raise ValueError("far pointers are not supported")
        if a != 1:
            raise ValueError("expected list pointer")
        offset = _sign_extend_30((word >> 2) & 0x3FFFFFFF)
        c = (word >> 32) & 7
        d = word >> 35
        if c != expect_size:
            raise ValueError(f"expected list element size {expect_size}, found {c}")
        return _ListPointer(ptr_word + 1 + offset, c, d)

    def get_u16(self, word: int, field_index: int) -> int:
        return struct.unpack_from("<H", self._data, self._seg + word * WORD + field_index * 2)[0]

    def get_i32(self, word: int, field_index: int) -> int:
        return struct.unpack_from("<i", self._data, self._seg + word * WORD + field_index * 4)[0]

    def get_u32(self, word: int, field_index: int) -> int:
        return struct.unpack_from("<I", self._data, self._seg + word * WORD + field_index * 4)[0]

    def get_i64(self, word: int, field_index: int) -> int:
        return struct.unpack_from("<q", self._data, self._seg + word * WORD + field_index * 8)[0]

    def get_u64(self, word: int, field_index: int) -> int:
        return struct.unpack_from("<Q", self._data, self._seg + word * WORD + field_index * 8)[0]

    def get_f32(self, word: int, field_index: int) -> float:
        return struct.unpack_from("<f", self._data, self._seg + word * WORD + field_index * 4)[0]

    def get_f64(self, word: int, field_index: int) -> float:
        return struct.unpack_from("<d", self._data, self._seg + word * WORD + field_index * 8)[0]

    def get_bool(self, word: int, bit_index: int) -> bool:
        byte_off = self._seg + word * WORD + (bit_index >> 3)
        if byte_off < 0 or byte_off >= len(self._data):
            raise ValueError("Cap'n pointer out of segment")
        return (self._data[byte_off] & (1 << (bit_index & 7))) != 0

    def read_byte_list(self, ptr_word: int) -> bytes:
        lp = self._list_pointer(ptr_word, 2)
        if lp is None:
            return b""
        n_words = (lp.length + WORD - 1) // WORD if lp.length else 0
        self._check_range(lp.target, n_words)
        start = self._seg + lp.target * WORD
        end = start + lp.length
        if end > len(self._data):
            raise ValueError("truncated Cap'n byte list")
        return self._data[start:end]

    def read_text(self, ptr_word: int) -> str:
        data = self.read_byte_list(ptr_word)
        if data.endswith(b"\x00"):
            data = data[:-1]
        return data.decode("utf-8")

    def read_cap_index(self, ptr_word: int) -> int | None:
        word = self.read_word(ptr_word)
        if word == 0:
            return None
        if word & 3 != 3:
            raise ValueError("expected capability pointer")
        return word >> 32

    def _pointer_list(self, ptr_word: int) -> list[int]:
        lp = self._list_pointer(ptr_word, 6)
        if lp is None or lp.length == 0:
            return []
        if lp.length > _MAX_TRAVERSAL_WORDS:
            raise ValueError("pointer list exceeds traversal budget")
        self._check_range(lp.target, lp.length)
        return [lp.target + i for i in range(lp.length)]

    def read_text_list(self, ptr_word: int) -> list[str]:
        return [self.read_text(w) for w in self._pointer_list(ptr_word)]

    def read_data_list(self, ptr_word: int) -> list[bytes]:
        return [self.read_byte_list(w) for w in self._pointer_list(ptr_word)]

    def read_uint_list(self, ptr_word: int, byte_width: int) -> list[int]:
        """Read a ``List(UInt16)`` (``byte_width=2``) or ``List(UInt32)`` (``4``)."""
        lp = self._list_pointer(ptr_word, 3 if byte_width == 2 else 4)
        if lp is None or lp.length == 0:
            return []
        n_words = (lp.length * byte_width + WORD - 1) // WORD
        self._check_range(lp.target, n_words)
        base = self._seg + lp.target * WORD
        return [
            int.from_bytes(
                self._data[base + i * byte_width : base + (i + 1) * byte_width],
                "little",
                signed=False,
            )
            for i in range(lp.length)
        ]

    def read_bool_list(self, ptr_word: int) -> list[bool]:
        lp = self._list_pointer(ptr_word, 1)
        if lp is None or lp.length == 0:
            return []
        n_words = (lp.length + 63) // 64
        self._check_range(lp.target, n_words)
        base = self._seg + lp.target * WORD
        return [(self._data[base + (i >> 3)] >> (i & 7)) & 1 == 1 for i in range(lp.length)]

    def read_struct_list(
        self, ptr_word: int, data_words: int, pointer_words: int
    ) -> list[_StructReader]:
        lp = self._list_pointer(ptr_word, 7)
        if lp is None or lp.length == 0:
            return []
        tag_word = lp.target
        self._check_range(tag_word, 1)
        tag = self.read_word(tag_word)
        count = (tag >> 2) & 0x3FFFFFFF
        dw = (tag >> 32) & 0xFFFF
        pw = (tag >> 48) & 0xFFFF
        elem_words = dw + pw
        if elem_words and count > lp.length // elem_words:
            raise ValueError("composite list count exceeds payload")
        if count > _MAX_TRAVERSAL_WORDS:
            raise ValueError("composite list count exceeds traversal budget")
        self._check_range(tag_word, 1 + count * elem_words)
        return [
            _StructReader(self, tag_word + 1 + i * elem_words, dw, pw) for i in range(count)
        ]


class _StructReader:
    """Struct cursor on a :class:`_CapnpReader`.

    Data fields outside the actual data section and pointers outside the
    actual pointer section decode as defaults (zero / empty / null), which is
    how Cap'n Proto reads a struct written by an older schema.
    """

    def __init__(
        self, reader: _CapnpReader, word: int, data_words: int, pointer_words: int
    ) -> None:
        self.reader = reader
        self.word = word
        self.data_words = data_words
        self.pointer_words = pointer_words

    def _has_data(self, field_index: int, size: int) -> bool:
        return (field_index + 1) * size <= self.data_words * WORD

    def pointer_word(self, index: int) -> int | None:
        if index >= self.pointer_words:
            return None
        return self.word + self.data_words + index

    def get_u16(self, field_index: int) -> int:
        return self.reader.get_u16(self.word, field_index) if self._has_data(field_index, 2) else 0

    def get_i32(self, field_index: int) -> int:
        return self.reader.get_i32(self.word, field_index) if self._has_data(field_index, 4) else 0

    def get_u32(self, field_index: int) -> int:
        return self.reader.get_u32(self.word, field_index) if self._has_data(field_index, 4) else 0

    def get_i64(self, field_index: int) -> int:
        return self.reader.get_i64(self.word, field_index) if self._has_data(field_index, 8) else 0

    def get_u64(self, field_index: int) -> int:
        return self.reader.get_u64(self.word, field_index) if self._has_data(field_index, 8) else 0

    def get_f32(self, field_index: int) -> float:
        return self.reader.get_f32(self.word, field_index) if self._has_data(field_index, 4) else 0.0

    def get_f64(self, field_index: int) -> float:
        return self.reader.get_f64(self.word, field_index) if self._has_data(field_index, 8) else 0.0

    def get_bool(self, bit_index: int) -> bool:
        if bit_index >= self.data_words * 64:
            return False
        return self.reader.get_bool(self.word, bit_index)

    def get_text(self, pointer_index: int) -> str:
        ptr = self.pointer_word(pointer_index)
        return "" if ptr is None else self.reader.read_text(ptr)

    def get_data(self, pointer_index: int) -> bytes:
        ptr = self.pointer_word(pointer_index)
        return b"" if ptr is None else self.reader.read_byte_list(ptr)

    def get_cap_index(self, pointer_index: int) -> int | None:
        ptr = self.pointer_word(pointer_index)
        return None if ptr is None else self.reader.read_cap_index(ptr)

    def get_text_list(self, pointer_index: int) -> list[str]:
        ptr = self.pointer_word(pointer_index)
        return [] if ptr is None else self.reader.read_text_list(ptr)

    def get_data_list(self, pointer_index: int) -> list[bytes]:
        ptr = self.pointer_word(pointer_index)
        return [] if ptr is None else self.reader.read_data_list(ptr)

    def get_bool_list(self, pointer_index: int) -> list[bool]:
        ptr = self.pointer_word(pointer_index)
        return [] if ptr is None else self.reader.read_bool_list(ptr)

    def get_u16_list(self, pointer_index: int) -> list[int]:
        ptr = self.pointer_word(pointer_index)
        return [] if ptr is None else self.reader.read_uint_list(ptr, 2)

    def get_u32_list(self, pointer_index: int) -> list[int]:
        ptr = self.pointer_word(pointer_index)
        return [] if ptr is None else self.reader.read_uint_list(ptr, 4)

    def get_struct_list(
        self, pointer_index: int, data_words: int, pointer_words: int
    ) -> list[_StructReader]:
        ptr = self.pointer_word(pointer_index)
        if ptr is None:
            return []
        return self.reader.read_struct_list(ptr, data_words, pointer_words)

    def get_struct(
        self, pointer_index: int, data_words: int, pointer_words: int
    ) -> _StructReader:
        ptr = self.pointer_word(pointer_index)
        if ptr is None:
            return _StructReader(self.reader, 0, 0, 0)
        return self.reader.struct_at(ptr, data_words, pointer_words)


__all__ = [
    "NO_CAPS",
    "WORD",
    "_CapTable",
    "_CapnpMessage",
    "_CapnpReader",
    "_CapnpStruct",
    "_StructReader",
]
