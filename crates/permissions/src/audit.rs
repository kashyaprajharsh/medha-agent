use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
};

pub(crate) fn field(value: &str) -> String {
    let encoded = serde_json::to_string(value).expect("string serialization");
    encoded[1..encoded.len() - 1].replace('|', "\\u007c")
}

pub(crate) fn append(path: &Path, record: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    let mut safe = metadata.is_file();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        safe &= metadata.nlink() == 1;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        safe &= metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            == 0;
        let mut information = std::mem::MaybeUninit::uninit();
        use std::os::windows::io::AsRawHandle;
        if unsafe {
            windows_sys::Win32::Storage::FileSystem::GetFileInformationByHandle(
                file.as_raw_handle(),
                information.as_mut_ptr(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        safe &= unsafe { information.assume_init() }.nNumberOfLinks == 1;
    }
    if !safe {
        return Err(io::Error::other(
            "audit log must be a regular file without links; repair the machine-local audit.log",
        ));
    }
    let mut locked = fd_lock::RwLock::new(file);
    locked.write()?.write_all(record.as_bytes())
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
