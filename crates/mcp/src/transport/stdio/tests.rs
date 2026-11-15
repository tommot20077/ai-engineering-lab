use super::*;

async fn read(input: &[u8], limit: usize) -> Vec<Result<Option<Vec<u8>>, LineError>> {
    // `&[u8]` 本身就實作 `AsyncBufRead`:不必真的開 pipe,就能測一行一行讀的規則。
    let mut reader = input;
    let mut out = Vec::new();
    loop {
        let line = read_message(&mut reader, limit).await;
        let stop = !matches!(line, Ok(Some(_)));
        out.push(line);
        if stop {
            return out;
        }
    }
}

#[tokio::test]
async fn messages_are_split_on_newlines() {
    assert_eq!(
        read(b"{\"a\":1}\n{}\n", 100).await,
        vec![
            Ok(Some(b"{\"a\":1}".to_vec())),
            Ok(Some(b"{}".to_vec())),
            Ok(None)
        ]
    );
    assert_eq!(read(b"", 100).await, vec![Ok(None)]);
}

#[tokio::test]
async fn the_limit_counts_the_bytes_before_the_newline() {
    // 剛好 5 個位元組:可以。
    assert_eq!(
        read(b"abcde\n", 5).await,
        vec![Ok(Some(b"abcde".to_vec())), Ok(None)]
    );
    // 6 個:超過。不必等到換行(對方可能永遠不送換行),讀到第 6 個就知道了。
    assert_eq!(read(b"abcdef\n", 5).await, vec![Err(LineError::TooLarge)]);
    assert_eq!(read(b"abcdefghij", 5).await, vec![Err(LineError::TooLarge)]);
}

#[tokio::test]
async fn a_line_cut_off_by_the_end_of_input_is_truncated() {
    assert_eq!(
        read(b"{}\n{\"jsonrpc\":", 100).await,
        vec![Ok(Some(b"{}".to_vec())), Err(LineError::Truncated)]
    );
    // 剛好在上限,沒有換行就結束:還是「斷在一半」,不是太大。
    assert_eq!(read(b"abcde", 5).await, vec![Err(LineError::Truncated)]);
}

#[test]
fn the_stderr_tail_keeps_only_the_last_bytes() {
    let mut tail = Tail::new(8);
    tail.push(b"0123");
    assert_eq!(tail.text(), "0123");
    tail.push(b"456789ab");
    assert_eq!(tail.text(), "456789ab");
    tail.push(b"c");
    assert_eq!(tail.text(), "56789abc");
}

#[test]
fn the_stderr_tail_is_made_printable() {
    // stderr 會被印到使用者的終端機:ESC 之類的控制字元可以改游標、改顏色,甚至讓前面的訊息看不見。
    // 換行與 tab 保留,`\r` 拿掉(Windows 的換行),其他換成 `?`;頭尾的空白去掉。
    let mut tail = Tail::new(100);
    tail.push(b"  \x1b[2Jerror:\tbad\r\nnext\x07 \n");
    assert_eq!(tail.text(), "?[2Jerror:\tbad\nnext?");
}

#[test]
fn the_stderr_tail_survives_a_split_character() {
    // 從中間切開的 UTF-8 字元換成 U+FFFD,不會讓整段變成錯誤。
    let mut tail = Tail::new(4);
    tail.push("a錯".as_bytes()); // 1 + 3 個位元組
    tail.push("x".as_bytes());
    assert_eq!(tail.text(), "錯x");
    tail.push("y".as_bytes());
    assert_eq!(tail.text(), "\u{fffd}\u{fffd}xy");
}
