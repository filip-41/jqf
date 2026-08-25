//! The 256-entry `MessagePack` marker table.
//!
//! Every first-byte family of the `MessagePack` spec maps to exactly one [`Marker`] row, generated once by [`MARKERS`].
//! `0xc1` is the spec's never-used byte and maps to [`Marker::NeverUsed`], which the scan rejects at head time — the
//! one marker-table entry that must exist so it cannot be forgotten.

/// The `MessagePack` first-byte family of one marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Marker {
    /// `0x00..=0x7f`: a positive fixint carrying its value directly.
    PosFixint,
    /// `0x80..=0x8f`: a fixmap whose count is the low nibble.
    Fixmap,
    /// `0x90..=0x9f`: a fixarray whose count is the low nibble.
    Fixarray,
    /// `0xa0..=0xbf`: a fixstr whose byte count is the low five bits.
    Fixstr,
    /// `0xc0`: nil.
    Nil,
    /// `0xc1`: never used — rejected at head time.
    NeverUsed,
    /// `0xc2`: false.
    False,
    /// `0xc3`: true.
    True,
    /// `0xc4`: bin8.
    Bin8,
    /// `0xc5`: bin16.
    Bin16,
    /// `0xc6`: bin32.
    Bin32,
    /// `0xc7`: ext8.
    Ext8,
    /// `0xc8`: ext16.
    Ext16,
    /// `0xc9`: ext32.
    Ext32,
    /// `0xca`: float32.
    Float32,
    /// `0xcb`: float64.
    Float64,
    /// `0xcc`: uint8.
    Uint8,
    /// `0xcd`: uint16.
    Uint16,
    /// `0xce`: uint32.
    Uint32,
    /// `0xcf`: uint64.
    Uint64,
    /// `0xd0`: int8.
    Int8,
    /// `0xd1`: int16.
    Int16,
    /// `0xd2`: int32.
    Int32,
    /// `0xd3`: int64.
    Int64,
    /// `0xd4`: fixext1.
    Fixext1,
    /// `0xd5`: fixext2.
    Fixext2,
    /// `0xd6`: fixext4.
    Fixext4,
    /// `0xd7`: fixext8.
    Fixext8,
    /// `0xd8`: fixext16.
    Fixext16,
    /// `0xd9`: str8.
    Str8,
    /// `0xda`: str16.
    Str16,
    /// `0xdb`: str32.
    Str32,
    /// `0xdc`: array16.
    Array16,
    /// `0xdd`: array32.
    Array32,
    /// `0xde`: map16.
    Map16,
    /// `0xdf`: map32.
    Map32,
    /// `0xe0..=0xff`: a negative fixint.
    NegFixint,
}

impl Marker {
    /// The count an embedded-length family carries in its first byte (fixmap low nibble, fixarray low nibble, fixstr
    /// low five bits, positive fixint the byte itself). Zero for every family whose length follows the head.
    #[must_use]
    pub(crate) const fn embedded_count(self, byte: u8) -> u64 {
        match self {
            Self::Fixmap | Self::Fixarray => (byte & 0x0f) as u64,
            Self::Fixstr => (byte & 0x1f) as u64,
            Self::PosFixint => byte as u64,
            _ => 0,
        }
    }

    /// The negative fixint's value: `byte - 256` (`0xe0` is `-32`, `0xff` is `-1`).
    #[must_use]
    pub(crate) const fn negative_fixint(self, byte: u8) -> i64 {
        debug_assert!(matches!(self, Self::NegFixint));
        byte as i8 as i64
    }
}

/// The generated 256-entry table. Built once with a const loop: each first byte's family is a closed function of the
/// byte, so the table is an exact finite map the scan dispatches on.
pub(crate) const MARKERS: [Marker; 256] = build_marker_table();

const fn build_marker_table() -> [Marker; 256] {
    let mut table = [Marker::NeverUsed; 256];
    let mut index = 0;
    while index < 256 {
        let byte = index as u8;
        table[index] = match byte {
            0x00..=0x7f => Marker::PosFixint,
            0x80..=0x8f => Marker::Fixmap,
            0x90..=0x9f => Marker::Fixarray,
            0xa0..=0xbf => Marker::Fixstr,
            0xc0 => Marker::Nil,
            0xc1 => Marker::NeverUsed,
            0xc2 => Marker::False,
            0xc3 => Marker::True,
            0xc4 => Marker::Bin8,
            0xc5 => Marker::Bin16,
            0xc6 => Marker::Bin32,
            0xc7 => Marker::Ext8,
            0xc8 => Marker::Ext16,
            0xc9 => Marker::Ext32,
            0xca => Marker::Float32,
            0xcb => Marker::Float64,
            0xcc => Marker::Uint8,
            0xcd => Marker::Uint16,
            0xce => Marker::Uint32,
            0xcf => Marker::Uint64,
            0xd0 => Marker::Int8,
            0xd1 => Marker::Int16,
            0xd2 => Marker::Int32,
            0xd3 => Marker::Int64,
            0xd4 => Marker::Fixext1,
            0xd5 => Marker::Fixext2,
            0xd6 => Marker::Fixext4,
            0xd7 => Marker::Fixext8,
            0xd8 => Marker::Fixext16,
            0xd9 => Marker::Str8,
            0xda => Marker::Str16,
            0xdb => Marker::Str32,
            0xdc => Marker::Array16,
            0xdd => Marker::Array32,
            0xde => Marker::Map16,
            0xdf => Marker::Map32,
            0xe0..=0xff => Marker::NegFixint,
        };
        index += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::{MARKERS, Marker};

    #[test]
    fn every_family_maps_to_its_byte() {
        assert_eq!(MARKERS[0x00], Marker::PosFixint);
        assert_eq!(MARKERS[0x7f], Marker::PosFixint);
        assert_eq!(MARKERS[0x80], Marker::Fixmap);
        assert_eq!(MARKERS[0x8f], Marker::Fixmap);
        assert_eq!(MARKERS[0x90], Marker::Fixarray);
        assert_eq!(MARKERS[0xa0], Marker::Fixstr);
        assert_eq!(MARKERS[0xbf], Marker::Fixstr);
        assert_eq!(MARKERS[0xc0], Marker::Nil);
        assert_eq!(MARKERS[0xc1], Marker::NeverUsed);
        assert_eq!(MARKERS[0xc2], Marker::False);
        assert_eq!(MARKERS[0xc3], Marker::True);
        assert_eq!(MARKERS[0xc4], Marker::Bin8);
        assert_eq!(MARKERS[0xc5], Marker::Bin16);
        assert_eq!(MARKERS[0xc6], Marker::Bin32);
        assert_eq!(MARKERS[0xc7], Marker::Ext8);
        assert_eq!(MARKERS[0xc8], Marker::Ext16);
        assert_eq!(MARKERS[0xc9], Marker::Ext32);
        assert_eq!(MARKERS[0xca], Marker::Float32);
        assert_eq!(MARKERS[0xcb], Marker::Float64);
        assert_eq!(MARKERS[0xcc], Marker::Uint8);
        assert_eq!(MARKERS[0xcd], Marker::Uint16);
        assert_eq!(MARKERS[0xce], Marker::Uint32);
        assert_eq!(MARKERS[0xcf], Marker::Uint64);
        assert_eq!(MARKERS[0xd0], Marker::Int8);
        assert_eq!(MARKERS[0xd1], Marker::Int16);
        assert_eq!(MARKERS[0xd2], Marker::Int32);
        assert_eq!(MARKERS[0xd3], Marker::Int64);
        assert_eq!(MARKERS[0xd4], Marker::Fixext1);
        assert_eq!(MARKERS[0xd5], Marker::Fixext2);
        assert_eq!(MARKERS[0xd6], Marker::Fixext4);
        assert_eq!(MARKERS[0xd7], Marker::Fixext8);
        assert_eq!(MARKERS[0xd8], Marker::Fixext16);
        assert_eq!(MARKERS[0xd9], Marker::Str8);
        assert_eq!(MARKERS[0xda], Marker::Str16);
        assert_eq!(MARKERS[0xdb], Marker::Str32);
        assert_eq!(MARKERS[0xdc], Marker::Array16);
        assert_eq!(MARKERS[0xdd], Marker::Array32);
        assert_eq!(MARKERS[0xde], Marker::Map16);
        assert_eq!(MARKERS[0xdf], Marker::Map32);
        assert_eq!(MARKERS[0xe0], Marker::NegFixint);
        assert_eq!(MARKERS[0xff], Marker::NegFixint);
    }

    #[test]
    fn embedded_counts_are_the_fixed_widths() {
        assert_eq!(MARKERS[0x85].embedded_count(0x85), 5);
        assert_eq!(MARKERS[0x93].embedded_count(0x93), 3);
        assert_eq!(MARKERS[0xaf].embedded_count(0xaf), 15);
        assert_eq!(MARKERS[0xbf].embedded_count(0xbf), 31);
        assert_eq!(MARKERS[0x7f].embedded_count(0x7f), 127);
        assert_eq!(MARKERS[0x01].embedded_count(0x01), 1);
        assert_eq!(MARKERS[0xca].embedded_count(0xca), 0);
    }

    #[test]
    fn negative_fixints_spell_minus_one_to_minus_thirty_two() {
        assert_eq!(MARKERS[0xff].negative_fixint(0xff), -1);
        assert_eq!(MARKERS[0xe0].negative_fixint(0xe0), -32);
    }

    #[test]
    fn the_never_used_byte_is_the_only_reject() {
        let count = MARKERS.iter().filter(|marker| **marker == Marker::NeverUsed).count();
        assert_eq!(count, 1, "exactly 0xc1 is never used");
    }
}
