use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    raw: Vec<u8>,
    header: Range<usize>,
    body: Range<usize>,
}

impl Message {
    pub fn from_bytes(raw: Vec<u8>) -> Self {
        let body_start = find_body_start(&raw).unwrap_or(raw.len());

        Self {
            header: 0..body_start,
            body: body_start..raw.len(),
            raw,
        }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.raw
    }

    pub fn header(&self) -> &[u8] {
        &self.raw[self.header.clone()]
    }

    pub fn body(&self) -> &[u8] {
        &self.raw[self.body.clone()]
    }

    pub fn len(&self) -> usize {
        self.raw.len()
    }

    pub fn is_empty(&self) -> bool {
        self.raw.is_empty()
    }
}

fn find_body_start(raw: &[u8]) -> Option<usize> {
    raw.windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|index| index + 2)
        })
}

#[cfg(test)]
mod tests {
    use super::Message;

    #[test]
    fn splits_lf_message() {
        let message = Message::from_bytes(b"Subject: test\n\nbody\n".to_vec());

        assert_eq!(message.header(), b"Subject: test\n\n");
        assert_eq!(message.body(), b"body\n");
    }

    #[test]
    fn splits_crlf_message() {
        let message = Message::from_bytes(b"Subject: test\r\n\r\nbody\r\n".to_vec());

        assert_eq!(message.header(), b"Subject: test\r\n\r\n");
        assert_eq!(message.body(), b"body\r\n");
    }

    #[test]
    fn preserves_binary_input() {
        let raw = b"X-Binary: yes\n\n\0\xff\n".to_vec();
        let message = Message::from_bytes(raw.clone());

        assert_eq!(message.as_bytes(), raw);
        assert_eq!(message.body(), b"\0\xff\n");
    }

    #[test]
    fn treats_message_without_separator_as_headers() {
        let message = Message::from_bytes(b"Subject: test\n".to_vec());

        assert_eq!(message.header(), b"Subject: test\n");
        assert!(message.body().is_empty());
    }
}
