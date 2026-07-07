//! 通用 box 遍历工具。ISO-BMFF box: `size(4) type(4) [largesize(8)] payload`。

/// 一个 box 的视图。
#[derive(Debug, Clone, Copy)]
pub struct BoxView<'a> {
    pub typ: [u8; 4],
    /// box 内容（不含 size/type 头）。
    pub payload: &'a [u8],
    /// 整个 box（含头）在父 buffer 中的字节范围长度。
    pub total_size: usize,
}

impl<'a> BoxView<'a> {
    pub fn type_str(&self) -> String {
        String::from_utf8_lossy(&self.typ).to_string()
    }
}

/// 遍历 `data` 顶层的所有 box，对每个调用 `f`。
pub fn iter_boxes(data: &[u8]) -> Vec<BoxView<'_>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 8 <= data.len() {
        let mut size =
            u32::from_be_bytes([data[p], data[p + 1], data[p + 2], data[p + 3]]) as usize;
        let typ = [data[p + 4], data[p + 5], data[p + 6], data[p + 7]];
        let mut hdr = 8;
        if size == 1 {
            if p + 16 > data.len() {
                break;
            }
            size = u64::from_be_bytes([
                data[p + 8],
                data[p + 9],
                data[p + 10],
                data[p + 11],
                data[p + 12],
                data[p + 13],
                data[p + 14],
                data[p + 15],
            ]) as usize;
            hdr = 16;
        }
        if size < hdr || p + size > data.len() {
            // size==0 表示到文件末尾
            if size == 0 {
                let payload = &data[p + hdr..];
                out.push(BoxView {
                    typ,
                    payload,
                    total_size: data.len() - p,
                });
            }
            break;
        }
        out.push(BoxView {
            typ,
            payload: &data[p + hdr..p + size],
            total_size: size,
        });
        p += size;
    }
    out
}

/// 在 `data` 中深度优先查找第一个指定类型的 box，返回其 payload。
/// `container_types` 指定哪些类型是容器（需递归进入）。
pub fn find_box<'a>(
    data: &'a [u8],
    target: &[u8; 4],
    container_types: &[&[u8; 4]],
) -> Option<&'a [u8]> {
    for b in iter_boxes(data) {
        if &b.typ == target {
            return Some(b.payload);
        }
        if container_types.contains(&&b.typ) {
            if let Some(found) = find_box(b.payload, target, container_types) {
                return Some(found);
            }
        }
    }
    None
}

/// 常见容器 box 集合。
pub const CONTAINERS: &[&[u8; 4]] = &[
    b"moov", b"trak", b"mdia", b"minf", b"stbl", b"mvex", b"moof", b"traf", b"sinf", b"schi",
    b"stsd", b"edts", b"dinf",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_boxes() {
        // ftyp(8空) + free(8空)
        let mut data = Vec::new();
        data.extend_from_slice(&8u32.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(&12u32.to_be_bytes());
        data.extend_from_slice(b"free");
        data.extend_from_slice(&[1, 2, 3, 4]);
        let boxes = iter_boxes(&data);
        assert_eq!(boxes.len(), 2);
        assert_eq!(&boxes[0].typ, b"ftyp");
        assert_eq!(&boxes[1].typ, b"free");
        assert_eq!(boxes[1].payload, &[1, 2, 3, 4]);
    }
}
