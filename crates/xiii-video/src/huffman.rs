//! Huffman machinery for the Bink video bundles.
//!
//! Per the MultimediaWiki *Bink Video* prose page there are 16 predefined tree *shapes*: the
//! frame stores only the tree number and a permutation of the 16 leaf symbols. The code lengths
//! behind each tree number are fixed data that this crate reads from the installation's
//! `binkw32.dll` (see [`crate::tables`]). Tree 0 is special: raw 4-bit nibbles with no code.
//!
//! Decoding is a plain canonical-Huffman walk: codes are assigned by increasing length, ties in
//! leaf order, and the first bit read is the most significant bit of the code.

use crate::bitreader::BitReader;
use crate::error::{Result, VideoError, VideoErrorKind};
use crate::tables::HuffmanLengths;

/// A binary node used for decoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Node {
    #[default]
    Empty,
    /// Internal node: left child (bit 0) and right child (bit 1).
    Branch(u16, u16),
    /// Leaf carrying a symbol.
    Leaf(u8),
}

/// A ready-to-decode Huffman tree: fixed shape plus the frame's symbol permutation.
#[derive(Debug, Clone)]
pub struct HuffmanTree {
    nodes: Vec<Node>,
    root: u16,
}

impl HuffmanTree {
    /// Builds a tree from a fixed code-length row and the frame's 16-symbol permutation.
    pub fn build(lens: &[u8; 16], syms: &[u8; 16]) -> Result<Self> {
        let mut count = [0u32; 16];
        for &l in lens.iter() {
            if l > 15 {
                return Err(VideoError::new(
                    VideoErrorKind::BadTable,
                    format!("Huffman code length {l} > 15"),
                ));
            }
        }
        // A code length of zero means "unused" (only tree 0 uses all-4 lengths).
        if lens.iter().all(|&l| l != 0) {
            for &l in lens.iter() {
                count[l as usize] += 1;
            }
        } else {
            for &l in lens.iter() {
                if l != 0 {
                    count[l as usize] += 1;
                }
            }
        }
        // Kraft check: a complete prefix code must exactly fill the code space.
        let mut kraft = 0u64;
        for (bits, &c) in count.iter().enumerate().skip(1) {
            kraft += u64::from(c) << (15 - bits);
        }
        if !lens.iter().all(|&l| l == 4) && kraft != 1 << 15 && kraft != 0 {
            return Err(VideoError::new(
                VideoErrorKind::BadTable,
                format!("Huffman code lengths do not satisfy Kraft equality (sum {kraft})"),
            ));
        }

        // Canonical codes: increasing length, ties in leaf order.
        let mut next_code = [0u32; 16];
        let mut code = 0u32;
        for bits in 1..16 {
            code = (code + count[bits - 1]) << 1;
            next_code[bits] = code;
        }

        let mut nodes = vec![Node::Empty];
        let root = 0u16;
        for (p, &l) in lens.iter().enumerate() {
            if l == 0 {
                continue;
            }
            let c = next_code[l as usize];
            next_code[l as usize] += 1;
            insert(&mut nodes, root, c, l as usize, syms[p])?;
        }
        debug_assert!(nodes[0] != Node::Empty);
        Ok(HuffmanTree { nodes, root })
    }

    /// Decodes one symbol from the bit reader.
    pub fn decode(&self, br: &mut BitReader<'_>) -> u8 {
        let mut node = self.root;
        loop {
            match self.nodes[node as usize] {
                Node::Leaf(s) => return s,
                Node::Branch(l, r) => {
                    let bit = br.bit();
                    node = if bit == 0 { l } else { r };
                }
                Node::Empty => return 0,
            }
        }
    }
}

/// Sentinel for a missing child.
const EMPTY: u16 = u16::MAX;

/// Inserts one leaf reachable by the `len`-bit code `code` (most significant bit first).
fn insert(nodes: &mut Vec<Node>, root: u16, code: u32, len: usize, sym: u8) -> Result<()> {
    let mut node = root;
    for i in 0..len {
        let bit = (code >> (len - 1 - i)) & 1;
        if matches!(nodes[node as usize], Node::Leaf(_)) {
            return Err(VideoError::new(
                VideoErrorKind::BadTable,
                "Huffman code is a prefix of another code",
            ));
        }
        let (mut l, mut r) = match nodes[node as usize] {
            Node::Branch(l, r) => (l, r),
            _ => (EMPTY, EMPTY),
        };
        let child = if bit == 0 { l } else { r };
        let is_last = i == len - 1;
        if child == EMPTY {
            let new = nodes.len() as u16;
            nodes.push(if is_last {
                Node::Leaf(sym)
            } else {
                Node::Empty
            });
            if bit == 0 {
                l = new;
            } else {
                r = new;
            }
            nodes[node as usize] = Node::Branch(l, r);
            if is_last {
                return Ok(());
            }
            node = new;
        } else if is_last {
            if nodes[child as usize] != Node::Empty {
                return Err(VideoError::new(
                    VideoErrorKind::BadTable,
                    "duplicate Huffman code",
                ));
            }
            nodes[child as usize] = Node::Leaf(sym);
            return Ok(());
        } else {
            node = child;
        }
    }
    Err(VideoError::new(
        VideoErrorKind::BadTable,
        "empty Huffman code",
    ))
}

/// A precomputed lookup-table Huffman decoder, matching the shipped decoder's representation.
///
/// `table[idx]` packs `(code_length << 4) | leaf_position` for the next `maxbits` bits; the leaf
/// position indexes the frame's symbol permutation. Tree 0 (raw nibbles) is naturally represented
/// as a width-4 table whose entries are `(4 << 4) | idx`.
#[derive(Debug, Clone)]
pub struct StaticTree {
    table: Vec<u8>,
    maxbits: usize,
    syms: [u8; 16],
}

impl StaticTree {
    /// Builds a decoder from a DLL lookup table and the frame's symbol permutation.
    pub fn new(table: &[u8], syms: [u8; 16]) -> Result<Self> {
        if table.is_empty() || !table.len().is_power_of_two() {
            return Err(VideoError::new(
                VideoErrorKind::BadTable,
                format!(
                    "Huffman lookup table size {} is not a power of two",
                    table.len()
                ),
            ));
        }
        Ok(Self {
            table: table.to_vec(),
            maxbits: table.len().trailing_zeros() as usize,
            syms,
        })
    }

    /// Table width in bits.
    pub fn maxbits(&self) -> usize {
        self.maxbits
    }

    /// Decodes one symbol.
    pub fn decode(&self, br: &mut BitReader<'_>) -> u8 {
        let idx = br.peek(self.maxbits) as usize;
        let e = self.table.get(idx).copied().unwrap_or(0);
        let len = (e >> 4) as usize;
        if len > 0 {
            br.skip(len);
        }
        self.syms[(e & 0x0f) as usize]
    }
}

/// A set of 16 ready trees indexed by the frame's tree number, plus the raw-nibble case.
#[derive(Debug, Clone)]
pub struct TreeSet {
    trees: Vec<Option<HuffmanTree>>,
}

impl TreeSet {
    /// Builds a tree set: tree 0 is raw nibbles (`None`), trees 1.. use `lengths`.
    pub fn build(lengths: &HuffmanLengths, tree_num: u8, syms: &[u8; 16]) -> Result<Self> {
        let mut trees = Vec::with_capacity(16);
        trees.push(None);
        for t in 1..16u8 {
            trees.push(Some(HuffmanTree::build(&lengths.rows[t as usize], syms)?));
        }
        let _ = tree_num;
        Ok(TreeSet { trees })
    }

    /// Decodes a symbol for tree number `tree_num` (already validated to 0..=15).
    pub fn decode(&self, tree_num: u8, br: &mut BitReader<'_>) -> u8 {
        if tree_num == 0 {
            return br.read(4) as u8;
        }
        match self.trees.get(tree_num as usize).and_then(|t| t.as_ref()) {
            Some(t) => t.decode(br),
            None => br.read(4) as u8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_two_symbol_tree() {
        // Lengths: symbol 0 gets 1 bit, symbol 1 gets 1 bit, rest unused.
        let mut lens = [0u8; 16];
        lens[0] = 1;
        lens[1] = 1;
        let syms = std::array::from_fn(|i| i as u8);
        let t = HuffmanTree::build(&lens, &syms).unwrap();
        // bit 0 -> symbol 0, bit 1 -> symbol 1.
        let data = [0b0000_0001u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br), 1);
        assert_eq!(t.decode(&mut br), 0);
    }

    #[test]
    fn applies_symbol_permutation() {
        let mut lens = [0u8; 16];
        lens[0] = 1;
        lens[1] = 1;
        // Leaf order 0 -> symbol 9, leaf 1 -> symbol 3.
        let mut syms = [0u8; 16];
        syms[0] = 9;
        syms[1] = 3;
        let t = HuffmanTree::build(&lens, &syms).unwrap();
        let data = [0b0000_0001u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br), 3);
        assert_eq!(t.decode(&mut br), 9);
    }

    #[test]
    fn rejects_incomplete_code() {
        // Only one 1-bit code leaves half the space unused.
        let mut lens = [0u8; 16];
        lens[0] = 1;
        assert!(HuffmanTree::build(&lens, &[0u8; 16]).is_err());
    }

    #[test]
    fn static_tree_reads_lookup() {
        // Width 2, LSB-first index: first bit 0 -> 1-bit code -> symbol 7 (even indices);
        // first bit 1 -> 2-bit code -> symbol 9 (odd indices).
        let table = [0x10u8, 0x21, 0x10, 0x21];
        let mut syms = [0u8; 16];
        syms[0] = 7;
        syms[1] = 9;
        let t = StaticTree::new(&table, syms).unwrap();
        // bits LSB-first: 0b0000_0001 => first bit 1, then 0,0 -> code "10"
        let data = [0b0000_0001u8];
        let mut br = BitReader::new(&data);
        assert_eq!(t.decode(&mut br), 9);
        assert_eq!(br.bits_read(), 2);
    }

    #[test]
    fn static_tree_rejects_non_power_of_two() {
        assert!(StaticTree::new(&[0u8; 3], [0u8; 16]).is_err());
    }
}
