use crate::option::BuildId;
use crate::utils::all_but_last_assert;
use itertools::Itertools;
use miette::Diagnostic;
use smallvec::{SmallVec, smallvec};
use std::collections::{BTreeMap, btree_map};
use std::fmt::Write as FmtWrite;
use std::io;
use std::io::Write;
use std::iter::{FusedIterator, Peekable};
use subslice_offset::SubsliceOffset;
use thiserror::Error;

#[derive(Clone, Debug)]
pub struct PatchVec {
    data: BTreeMap<u32, u8>,
}

impl PatchVec {
    pub fn new() -> Self {
        Self {
            data: BTreeMap::new(),
        }
    }

    pub fn put_byte(&mut self, address: u32, byte: u8) {
        self.data.insert(address, byte);
    }

    pub fn put(&mut self, mut address: u32, bytes: impl IntoIterator<Item = u8>) {
        for byte in bytes {
            self.put_byte(address, byte);
            address += 1;
        }
    }

    pub fn has_edit_at(&self, address: u32) -> bool {
        self.data.contains_key(&address)
    }

    pub fn has_hunk_starting_at(&self, address: u32) -> bool {
        if address == 0 {
            return self.data.contains_key(&0);
        }
        self.data
            .range(address - 1..=address)
            .next()
            .is_some_and(|(&first_addr, _)| first_addr == address)
    }

    pub fn iter_hunks(&self, max_hunk_size: u32) -> HunksIterator<btree_map::Iter<'_, u32, u8>> {
        assert!(max_hunk_size > 0, "max_hunk_size cannot be 0");
        HunksIterator {
            inner: self.data.iter().peekable(),
            max_size: max_hunk_size,
        }
    }
}

pub struct HunksIterator<I: Iterator> {
    inner: Peekable<I>,
    max_size: u32,
}

impl<'a, I> Iterator for HunksIterator<I>
where
    I: Iterator<Item = (&'a u32, &'a u8)>,
{
    type Item = (u32, SmallVec<[u8; 4]>);

    fn next(&mut self) -> Option<Self::Item> {
        let (&base_addr, &first) = self.inner.next()?;
        let mut result = smallvec![first];
        let mut current_addr = base_addr + 1;
        while (result.len() as u32) < self.max_size
            && let Some((_, &next)) = self.inner.next_if(|&(&addr, _)| addr == current_addr)
        {
            result.push(next);
            current_addr += 1;
        }
        Some((base_addr, result))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let (lo, hi) = self.inner.size_hint();
        (lo.min(1), hi)
    }
}

impl<'a, I> FusedIterator for HunksIterator<I> where I: Iterator<Item = (&'a u32, &'a u8)> {}

pub fn generate_pchtxt(
    vec: &PatchVec,
    build_id: BuildId,
    mut output: impl Write,
) -> io::Result<()> {
    fn hunk_len_as_string(hunk: &[u8]) -> Option<usize> {
        if hunk.last() != Some(&0) {
            return None;
        }
        let mut len = 2;
        for &byte in &hunk[..hunk.len() - 1] {
            match byte {
                b'\x07' | b'\x08' | b'\x0C' | b'\n' | b'\r' | b'\t' | b'\x0B' | b'\\' | b'"' => {
                    len += 2
                }
                _ if byte.is_ascii_control() => return None,
                _ => len += 1,
            }
        }
        str::from_utf8(hunk).is_ok().then_some(len)
    }

    #[inline]
    fn hunk_len_as_bytes(hunk: &[u8]) -> usize {
        hunk.len() * 2
    }

    let mut scratch = String::new();

    writeln!(output, "@nsobid-{build_id:032X}")?;
    writeln!(output, "@enabled")?;
    for (mut addr, hunk) in vec.iter_hunks(4080) {
        let mut sub_hunks = hunk
            .split_inclusive(|&x| x == 0)
            .map(|h| (h, hunk_len_as_string(h)))
            .flat_map::<SmallVec<[_; 2]>, _>(|(h, len_as_str)| {
                if len_as_str.is_some() || h.last() != Some(&0) {
                    return smallvec![(h, len_as_str)];
                }
                let Some(first_non_control) = h.iter().position(|x| !x.is_ascii_control()) else {
                    return smallvec![(h, len_as_str)];
                };
                if first_non_control == 0 {
                    return smallvec![(h, len_as_str)];
                }
                let (left, right) = h.split_at(first_non_control);
                let right_hunk_len = hunk_len_as_string(right);
                if right_hunk_len.is_some() {
                    // The left part can never be considered a string, as it doesn't end with \0
                    smallvec![(left, None), (right, right_hunk_len)]
                } else {
                    smallvec![(h, len_as_str)]
                }
            })
            .collect::<SmallVec<[_; 4]>>();

        let mut hunk_idx = 0;
        while hunk_idx < sub_hunks.len() - 1 {
            let (sub_hunk, len_as_string) = sub_hunks[hunk_idx];
            let (next_sub_hunk, next_len_as_string) = sub_hunks[hunk_idx + 1];
            let total_len_if_alone = len_as_string.unwrap_or(hunk_len_as_bytes(sub_hunk))
                + 10
                + next_len_as_string.unwrap_or(hunk_len_as_bytes(next_sub_hunk));
            let total_len_if_joined =
                hunk_len_as_bytes(sub_hunk) + hunk_len_as_bytes(next_sub_hunk);
            if total_len_if_joined < total_len_if_alone {
                let first_offset = hunk.subslice_offset(sub_hunk).unwrap();
                assert_eq!(
                    first_offset + sub_hunk.len(),
                    hunk.subslice_offset(next_sub_hunk).unwrap()
                );
                // Always None because such a joined hunk will always have a 0 byte in the middle
                sub_hunks[hunk_idx] = (
                    &hunk[first_offset..first_offset + sub_hunk.len() + next_sub_hunk.len()],
                    None,
                );
                sub_hunks.remove(hunk_idx + 1);
            } else {
                hunk_idx += 1;
            }
        }

        for (sub_hunk, len_as_string) in sub_hunks {
            let _ = write!(scratch, "{addr:08X} ");
            if len_as_string.is_some() {
                scratch.push('"');
                for char in all_but_last_assert(str::from_utf8(sub_hunk).unwrap().chars(), '\0') {
                    match char {
                        '\x07' => scratch.push_str(r"\a"),
                        '\x08' => scratch.push_str(r"\b"),
                        '\x0C' => scratch.push_str(r"\f"),
                        '\n' => scratch.push_str(r"\n"),
                        '\r' => scratch.push_str(r"\r"),
                        '\t' => scratch.push_str(r"\t"),
                        '\x0B' => scratch.push_str(r"\v"),
                        '\\' => scratch.push_str(r"\\"),
                        '"' => scratch.push_str(r#"\""#),
                        _ => scratch.push(char),
                    }
                }
                scratch.push('"');
            } else {
                for &byte in sub_hunk {
                    let _ = write!(scratch, "{byte:02X}");
                }
            }
            writeln!(output, "{scratch}")?;
            scratch.clear();
            addr += sub_hunk.len() as u32;
        }
    }
    Ok(())
}

const IPS_UNSAFE_OFFSET: u32 = 0x45454F46;
const IPS_MAX_HUNK_SIZE: u32 = 0xFFFF;

pub fn check_generate_ips(vec: &PatchVec) -> Result<(), IpsGenerateError> {
    if vec.has_hunk_starting_at(IPS_UNSAFE_OFFSET) {
        return Err(IpsGenerateError::HasEofHunk);
    }
    Ok(())
}

/// Returns whether the patch was written. If the patch contains a hunk starting at address
/// `0x45454F46`, it cannot be written.
pub fn generate_ips(vec: &PatchVec, mut output: impl Write) -> Result<(), IpsGenerateError> {
    #[inline]
    fn encode_hunk_offset(offset: u32, output: &mut impl Write) -> io::Result<()> {
        output.write_all(&offset.to_be_bytes())
    }

    #[inline]
    fn encode_hunk_len(offset: u32, len: usize, output: &mut impl Write) -> io::Result<()> {
        assert!(
            0 < len && len <= IPS_MAX_HUNK_SIZE as usize,
            "hunk at {offset} is {len}"
        );
        output.write_all(&(len as u16).to_be_bytes())
    }

    fn encode_standard_hunk(offset: u32, hunk: &[u8], output: &mut impl Write) -> io::Result<()> {
        encode_hunk_offset(offset, output)?;
        encode_hunk_len(offset, hunk.len(), output)?;
        output.write_all(hunk)?;
        Ok(())
    }

    fn encode_rle_hunk(
        offset: u32,
        byte: u8,
        count: usize,
        output: &mut impl Write,
    ) -> io::Result<()> {
        encode_hunk_offset(offset, output)?;
        output.write_all(&[0, 0])?;
        encode_hunk_len(offset, count, output)?;
        output.write_all(&[byte])?;
        Ok(())
    }

    if vec.has_hunk_starting_at(IPS_UNSAFE_OFFSET) {
        return Err(IpsGenerateError::HasEofHunk);
    }

    output.write_all(b"IPS32")?;
    let has_edit_at_unsafe_offset = vec.has_edit_at(IPS_UNSAFE_OFFSET);
    let mut safety_byte = None;
    for (mut addr, mut hunk) in vec.iter_hunks(IPS_MAX_HUNK_SIZE) {
        let extra_byte = if has_edit_at_unsafe_offset {
            if addr + hunk.len() as u32 == IPS_UNSAFE_OFFSET {
                assert!(safety_byte.is_none());
                assert_eq!(hunk.len(), IPS_MAX_HUNK_SIZE as usize);
                safety_byte = Some(hunk.remove(IPS_MAX_HUNK_SIZE as usize - 1));
                None
            } else if addr == IPS_UNSAFE_OFFSET {
                let extra_byte = (hunk.len() == IPS_MAX_HUNK_SIZE as usize)
                    .then(|| hunk.remove(IPS_MAX_HUNK_SIZE as usize - 1));
                hunk.insert(
                    0,
                    safety_byte
                        .take()
                        .expect("safety_byte should've been assigned by previous loop iteration"),
                );
                addr -= 1;
                extra_byte
            } else {
                None
            }
        } else {
            None
        };
        if hunk.len() <= 3 {
            encode_standard_hunk(addr, &hunk, &mut output)?;
            addr += hunk.len() as u32;
        } else if hunk.iter().all_equal() {
            encode_rle_hunk(addr, hunk[0], hunk.len(), &mut output)?;
            addr += hunk.len() as u32;
        } else if hunk.len() <= 14 {
            encode_standard_hunk(addr, &hunk, &mut output)?;
            addr += hunk.len() as u32;
        } else {
            let mut hunk = hunk.as_slice();
            if hunk[..10].iter().all_equal() {
                let value = hunk[0];
                let first_unequal = hunk[10..]
                    .iter()
                    .position(|&x| x != value)
                    .map_or(hunk.len(), |x| x + 10);
                encode_rle_hunk(addr, value, first_unequal, &mut output)?;
                addr += first_unequal as u32;
                hunk = &hunk[first_unequal..];
            }
            let mut finishing_hunk = None;
            if hunk.len() >= 10 && hunk[hunk.len() - 10..].iter().all_equal() {
                let value = hunk[hunk.len() - 1];
                let last_equal = hunk[..hunk.len() - 10]
                    .iter()
                    .rposition(|&x| x != value)
                    .map_or(0, |x| x + 1);
                finishing_hunk = Some((value, hunk.len() - last_equal));
                hunk = &hunk[..last_equal];
            }
            while !hunk.is_empty() {
                let Some(next_rle_group) = hunk
                    .array_windows::<15>()
                    .position(|a| a.iter().all_equal())
                else {
                    encode_standard_hunk(addr, hunk, &mut output)?;
                    addr += hunk.len() as u32;
                    hunk = &hunk[hunk.len()..];
                    break;
                };
                encode_standard_hunk(addr, &hunk[..next_rle_group], &mut output)?;
                addr += next_rle_group as u32;
                hunk = &hunk[next_rle_group..];
                let value = hunk[0];
                let first_unequal = hunk[15..]
                    .iter()
                    .position(|&x| x != value)
                    .map_or(hunk.len(), |x| x + 15);
                encode_rle_hunk(addr, value, first_unequal, &mut output)?;
                addr += first_unequal as u32;
                hunk = &hunk[first_unequal..];
            }
            if let Some((value, len)) = finishing_hunk {
                encode_rle_hunk(addr, value, len, &mut output)?;
                addr += len as u32;
            }
        }
        if let Some(byte) = extra_byte {
            encode_standard_hunk(addr, &[byte], &mut output)?;
        }
    }
    output.write_all(b"EEOF")?;
    Ok(())
}

#[derive(Debug, Diagnostic, Error)]
pub enum IpsGenerateError {
    #[error("IPS Patch with hunk starting at address 0x45454F46 cannot be written")]
    #[diagnostic(code(ips::eof_hunk))]
    HasEofHunk,
    #[error(transparent)]
    #[diagnostic(code(ips::io))]
    Io(#[from] io::Error),
}

impl IpsGenerateError {
    pub fn unwrap_io_err(self) -> io::Error {
        match self {
            Self::Io(err) => err,
            err => panic!("IpsGenerateError::unwrap_io_err called with non-I/O error: {err}"),
        }
    }
}

#[cfg(test)]
mod test {
    use crate::output::{PatchVec, generate_ips, generate_pchtxt};
    use pretty_assertions::assert_eq;
    use smallvec::smallvec;

    #[test]
    fn test_patch_vec() {
        let mut vec = PatchVec::new();
        vec.put_byte(10, 10);
        vec.put(50, [10, 20, 30]);
        vec.put(60, [40, 50, 60]);
        vec.put_byte(63, 100);

        let mut iter = vec.iter_hunks(100);
        assert_eq!(iter.size_hint(), (1, Some(8)));
        assert_eq!(iter.next(), Some((10, smallvec![10])));
        assert_eq!(iter.size_hint(), (1, Some(7)));
        assert_eq!(iter.next(), Some((50, smallvec![10, 20, 30])));
        assert_eq!(iter.size_hint(), (1, Some(4)));
        assert_eq!(iter.next(), Some((60, smallvec![40, 50, 60, 100])));
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.next(), None);

        assert!(vec.has_hunk_starting_at(50));
        assert!(!vec.has_hunk_starting_at(62));
        assert!(!vec.has_hunk_starting_at(0));
    }

    fn generate_patch_vec(patches: &[(u32, &[u8])]) -> PatchVec {
        let mut vec = PatchVec::new();
        for &(addr, patch) in patches {
            vec.put(addr, patch.iter().copied());
        }
        vec
    }

    #[test]
    fn test_generate_pchtxt() {
        fn generate(patches: &[(u32, &[u8])]) -> String {
            let mut result = vec![];
            generate_pchtxt(&generate_patch_vec(patches), 0, &mut result).unwrap();

            let mut str = String::from_utf8(result).unwrap();
            assert_eq!(
                &str[..49],
                "@nsobid-00000000000000000000000000000000\n@enabled",
            );
            assert_eq!(&str[str.len() - 1..], "\n");
            str.drain(..49);
            str.drain(str.len() - 1..);
            str
        }

        assert_eq!(
            generate(&[(0, &[50])]),
            r#"
00000000 32"#
        );
        assert_eq!(
            generate(&[(50, "Hello!\0".as_bytes())]),
            r#"
00000032 "Hello!""#
        );
        assert_eq!(
            generate(&[
                (50, "Hello!\0".as_bytes()),
                (57, "Multi\nline\nstring\twith\ttabs\0".as_bytes()),
            ]),
            r#"
00000032 "Hello!"
00000039 "Multi\nline\nstring\twith\ttabs""#
        );
        assert_eq!(
            generate(&[
                (50, "Hello!\0".as_bytes()),
                (57, &[10, 20]),
                (59, "Multi\nline\nstring\twith\ttabs\0".as_bytes()),
            ]),
            r#"
00000032 48656C6C6F21000A14
0000003B "Multi\nline\nstring\twith\ttabs""#
        );
    }

    #[test]
    fn test_generate_ips() {
        fn generate(patches: &[(u32, &[u8])]) -> Vec<u8> {
            let mut result = vec![];
            generate_ips(&generate_patch_vec(patches), &mut result).unwrap();

            assert_eq!(&result[..5], b"IPS32");
            assert_eq!(&result[result.len() - 4..], b"EEOF");
            result.drain(..5);
            result.drain(result.len() - 4..);
            result
        }

        assert_eq!(
            generate(&[(50, &[10, 20, 30])]),
            &[0, 0, 0, 50, 0, 3, 10, 20, 30],
        );
        assert_eq!(
            generate(&[(50, &[30, 30, 30])]),
            &[0, 0, 0, 50, 0, 3, 30, 30, 30],
        );

        assert_eq!(
            generate(&[(50, &[10, 20, 30, 40, 50, 60])]),
            &[0, 0, 0, 50, 0, 6, 10, 20, 30, 40, 50, 60],
        );
        assert_eq!(
            generate(&[(50, &[30, 30, 30, 30, 30, 30])]),
            &[0, 0, 0, 50, 0, 0, 0, 6, 30],
        );

        assert_eq!(
            generate(&[(
                50,
                &[10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24]
            )]),
            &[
                0, 0, 0, 50, 0, 15, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24
            ],
        );
        assert_eq!(
            generate(&[(50, &[10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10])]),
            &[0, 0, 0, 50, 0, 0, 0, 11, 10],
        );
        #[rustfmt::skip]
        assert_eq!(
            generate(&[(
                50,
                &[10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 21, 22, 23, 24]
            )]),
            &[
                0, 0, 0, 50, 0, 0, 0, 11, 10,
                0, 0, 0, 61, 0, 4, 21, 22, 23, 24,
            ],
        );
        #[rustfmt::skip]
        assert_eq!(
            generate(&[(
                50,
                &[
                    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 20, 20, 20, 20, 20, 20, 20, 20, 20,
                    20, 20,
                ]
            )]),
            &[
                0, 0, 0, 50, 0, 0, 0, 11, 10,
                0, 0, 0, 61, 0, 0, 0, 11, 20,
            ],
        );
        #[rustfmt::skip]
        assert_eq!(
            generate(&[(
                50,
                &[
                    10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 10, 12, 13, 14, 15, 16, 17, 18, 20, 20,
                    20, 20, 20, 20, 20, 20, 20, 20, 20,
                ]
            )]),
            &[
                0, 0, 0, 50, 0, 0, 0, 11, 10,
                0, 0, 0, 61, 0, 7, 12, 13, 14, 15, 16, 17, 18,
                0, 0, 0, 68, 0, 0, 0, 11, 20,
            ],
        );

        #[rustfmt::skip]
        assert_eq!(
            generate(&[(
                50,
                &[
                    10, 11, 12, 13, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50, 50,
                    80, 81, 82, 83,
                ]
            )]),
            &[
                0, 0, 0, 50, 0, 4, 10, 11, 12, 13,
                0, 0, 0, 54, 0, 0, 0, 16, 50,
                0, 0, 0, 70, 0, 4, 80, 81, 82, 83,
            ],
        );

        assert_eq!(generate(&[(0x45454F44, b"hello")]), b"EEOD\0\x05hello");
        // This offset is because 0x45444F46 + 0xFFFF ends right before 0x45454F46
        #[rustfmt::skip]
        assert_eq!(
            generate(&[(0x45444F47, &[18; 0x10010])]),
            &[
                0x45, 0x44, 0x4F, 0x47, 0, 0, 0xFF, 0xFE, 18,
                0x45, 0x45, 0x4F, 0x45, 0, 0, 0x00, 0x12, 18,
            ],
        );
        #[rustfmt::skip]
        assert_eq!(
            generate(&[(0x45444F47, &[18; 0x20010])]),
            &[
                0x45, 0x44, 0x4F, 0x47, 0, 0, 0xFF, 0xFE, 18,
                0x45, 0x45, 0x4F, 0x45, 0, 0, 0xFF, 0xFF, 18,
                0x45, 0x46, 0x4F, 0x44, 0, 1, 18,
                0x45, 0x46, 0x4F, 0x45, 0, 0, 0x00, 0x12, 18,
            ],
        );
    }
}
