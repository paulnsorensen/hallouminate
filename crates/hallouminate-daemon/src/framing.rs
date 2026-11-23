use tokio::io::AsyncRead;
use tokio_util::codec::{FramedRead, LinesCodec};

pub(crate) const MAX_IPC_LINE_BYTES: usize = 4 * 1024 * 1024;

pub(crate) fn ipc_lines<R>(reader: R) -> FramedRead<R, LinesCodec>
where
    R: AsyncRead,
{
    FramedRead::new(
        reader,
        LinesCodec::new_with_max_length(MAX_IPC_LINE_BYTES - 1),
    )
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;
    use tokio_util::codec::LinesCodecError;

    use super::*;

    async fn decode(bytes: Vec<u8>) -> Result<Option<String>, LinesCodecError> {
        let capacity = bytes.len().max(1);
        let (mut writer, reader) = tokio::io::duplex(capacity);
        writer.write_all(&bytes).await.expect("write input");
        drop(writer);
        ipc_lines(reader).next().await.transpose()
    }

    #[tokio::test]
    async fn line_below_newline_inclusive_byte_cap_is_accepted() {
        let mut bytes = vec![b'a'; MAX_IPC_LINE_BYTES - 2];
        bytes.push(b'\n');
        let line = decode(bytes).await.expect("decode line").expect("line");
        assert_eq!(line.len(), MAX_IPC_LINE_BYTES - 2);
    }

    #[tokio::test]
    async fn line_at_newline_inclusive_byte_cap_is_accepted() {
        let mut bytes = vec![b'a'; MAX_IPC_LINE_BYTES - 1];
        bytes.push(b'\n');
        let line = decode(bytes).await.expect("decode line").expect("line");
        assert_eq!(line.len(), MAX_IPC_LINE_BYTES - 1);
    }

    #[tokio::test]
    async fn line_above_newline_inclusive_byte_cap_is_rejected() {
        let mut bytes = vec![b'a'; MAX_IPC_LINE_BYTES];
        bytes.push(b'\n');
        let error = decode(bytes).await.expect_err("oversized line must fail");
        assert_eq!(error.to_string(), "max line length exceeded");
    }

    #[tokio::test]
    async fn lf_and_eof_both_finish_a_line() {
        let lf = decode(b"line\n".to_vec())
            .await
            .expect("decode LF line")
            .expect("LF line");
        let eof = decode(b"line".to_vec())
            .await
            .expect("decode EOF line")
            .expect("EOF line");
        assert_eq!(lf, "line");
        assert_eq!(eof, "line");
    }

    #[tokio::test]
    async fn invalid_utf8_is_rejected() {
        let error = decode(vec![0xff, b'\n'])
            .await
            .expect_err("invalid UTF-8 must fail");
        assert!(error.to_string().contains("Unable to decode input as UTF8"));
    }

    #[tokio::test]
    async fn empty_input_has_no_line() {
        let line = decode(Vec::new()).await.expect("decode empty input");
        assert_eq!(line, None);
    }
}
