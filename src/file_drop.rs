use gtk::{gio, glib};

pub fn parse_files(data: &[u8]) -> anyhow::Result<Vec<gio::File>> {
    let mut files = Vec::new();
    for uri in std::str::from_utf8(data)?.lines().map(str::trim) {
        if uri.is_empty() || uri.starts_with('#') { continue; }
        let (path, host) = glib::filename_from_uri(uri)?;
        anyhow::ensure!(host.as_deref().is_none_or(|host| host == "localhost"),
            "Only local files can be dropped");
        files.push(gio::File::for_path(path));
    }
    anyhow::ensure!(!files.is_empty(), "No files in drop");
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::prelude::*;

    #[test]
    fn uri_lists_decode_spaces_and_reject_remote_or_invalid_data() {
        let files = parse_files(b"# comment\r\nfile:///tmp/first%20file.txt\r\nfile:///tmp/second.txt\r\n").unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path().unwrap(), std::path::Path::new("/tmp/first file.txt"));
        for invalid in [b"".as_slice(), b"https://example.com/file", b"file://remote/tmp/file", b"\xff", b"not a URI"] {
            assert!(parse_files(invalid).is_err());
        }
    }
}
