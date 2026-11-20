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
    checked_boxes(data)
        .map_while(|item| item.map(|b| b.view))
        .collect()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LocatedBox<'a> {
    pub view: BoxView<'a>,
    pub start: usize,
    /// The whole box, including its standard or extended header.
    pub whole: &'a [u8],
}

/// Like `iter_boxes`, but exposes offsets and produces `None` for a truncated or
/// invalid last box rather than silently accepting a partial stream.
pub(crate) fn checked_boxes(mut data: &[u8]) -> impl Iterator<Item = Option<LocatedBox<'_>>> {
    let mut start = 0;
    std::iter::from_fn(move || {
        if data.is_empty() {
            return None;
        }
        let item = read_box(data, start);
        if let Some(b) = item {
            data = &data[b.whole.len()..];
            start += b.whole.len();
        } else {
            data = &[];
        }
        Some(item)
    })
}

fn read_box(data: &[u8], start: usize) -> Option<LocatedBox<'_>> {
    let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?);
    let typ = data.get(4..8)?.try_into().ok()?;
    let (size, header) = match size {
        0 => (data.len(), 8),
        1 => (
            usize::try_from(u64::from_be_bytes(data.get(8..16)?.try_into().ok()?)).ok()?,
            16,
        ),
        _ => (size as usize, 8),
    };
    let whole = data.get(..size)?;
    let payload = whole.get(header..)?;
    Some(LocatedBox {
        view: BoxView {
            typ,
            payload,
            total_size: size,
        },
        start,
        whole,
    })
}

/// 在 `data` 中深度优先查找第一个指定类型的 box，返回其 payload。
/// `container_types` 指定哪些类型是容器（需递归进入）。
pub fn find_box<'a>(
    data: &'a [u8],
    target: &[u8; 4],
    container_types: &[&[u8; 4]],
) -> Option<&'a [u8]> {
    find_box_with_limit(data, target, container_types, 16)
}

fn find_box_with_limit<'a>(
    data: &'a [u8],
    target: &[u8; 4],
    container_types: &[&[u8; 4]],
    depth: u8,
) -> Option<&'a [u8]> {
    for b in checked_boxes(data).map_while(|b| b.map(|b| b.view)) {
        if &b.typ == target {
            return Some(b.payload);
        }
        if depth > 0 && container_types.contains(&&b.typ) {
            if let Some(found) = find_box_with_limit(b.payload, target, container_types, depth - 1)
            {
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
