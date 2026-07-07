//! 大端按位写入器，用于打包 descriptor / PES header / PCR 等位域字段。

#[derive(Default)]
pub struct BitWriter {
    buf: Vec<u8>,
    /// 当前正在累积的字节
    cur: u8,
    /// cur 中已写入的位数 (0..8)
    nbits: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// 写入 `count` 位（count ≤ 32），取 `val` 的低 count 位，大端在前。
    pub fn bits(&mut self, val: u32, count: u8) {
        debug_assert!(count <= 32);
        for i in (0..count).rev() {
            let bit = ((val >> i) & 1) as u8;
            self.cur = (self.cur << 1) | bit;
            self.nbits += 1;
            if self.nbits == 8 {
                self.buf.push(self.cur);
                self.cur = 0;
                self.nbits = 0;
            }
        }
    }

    /// 写一个布尔位。
    pub fn bit(&mut self, b: bool) {
        self.bits(b as u32, 1);
    }

    /// 写一个对齐的字节（要求当前已在字节边界）。
    pub fn u8(&mut self, v: u8) {
        debug_assert_eq!(self.nbits, 0, "u8() requires byte alignment");
        self.buf.push(v);
    }

    /// 用指定填充位补齐到字节边界。
    pub fn align(&mut self, fill_bit: bool) {
        while self.nbits != 0 {
            self.bit(fill_bit);
        }
    }

    /// 当前是否在字节边界。
    pub fn is_aligned(&self) -> bool {
        self.nbits == 0
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.nbits == 0
    }

    /// 取出结果（要求已字节对齐）。
    pub fn into_bytes(mut self) -> Vec<u8> {
        if self.nbits != 0 {
            // 安全起见补 0
            self.align(false);
        }
        std::mem::take(&mut self.buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_dovi_fields() {
        // profile=5(7bit) level=6(6bit) rpu=1 el=0 bl=1 compat=0(4bit)
        // 期望与实测 dvcC payload 后3字节一致: 0a 35 0x
        let mut w = BitWriter::new();
        w.bits(5, 7); // profile
        w.bits(6, 6); // level
        w.bit(true); // rpu
        w.bit(false); // el
        w.bit(true); // bl
        w.bits(0, 4); // compat
        w.align(false);
        let out = w.into_bytes();
        // 7+6+1+1+1+4 = 20 bits → 3 字节
        assert_eq!(out[0], 0x0A);
        assert_eq!(out[1], 0x35);
        assert_eq!(out[2] & 0xF0, 0x00); // compat 高4位=0
    }
}
