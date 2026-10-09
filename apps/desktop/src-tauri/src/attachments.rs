//! Native selection of local message sources. Documents/folders remain path
//! references; images pass through the same bounded admission as clipboard images.
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek},
    path::PathBuf,
};

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Selection {
    Files,
    Folder,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Attachment {
    Image {
        name: String,
        mime: String,
        data: String,
        note: Option<String>,
    },
    File {
        name: String,
        path: String,
    },
    Folder {
        name: String,
        path: String,
    },
}

fn selected(paths: Vec<PathBuf>) -> Result<Vec<Attachment>, String> {
    if paths.len() > 16 {
        return Err("Attach up to sixteen files or folders at a time.".into());
    }
    let mut attachments = Vec::new();
    let mut images = 0;
    for path in paths {
        let text = path
            .to_str()
            .ok_or("This path cannot be represented as text")?
            .to_owned();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&text)
            .to_owned();
        let metadata = std::fs::metadata(&path).map_err(|error| format!("{name}: {error}"))?;
        if metadata.is_dir() {
            attachments.push(Attachment::Folder { name, path: text });
            continue;
        }
        if !metadata.is_file() {
            return Err(format!("{name} is not a regular file or folder."));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| format!("{name}: {error}"))?;
        // Check the opened handle too; a replaced path must not turn a header
        // probe into a blocking read of a pipe/device.
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if !metadata.is_file() {
            return Err(format!("{name} is not a regular file."));
        }
        let mut header = [0; 32];
        let count = file.read(&mut header).map_err(|error| error.to_string())?;
        if !media::has_supported_image_header(&header[..count]) {
            attachments.push(Attachment::File { name, path: text });
            continue;
        }
        images += 1;
        if images > 4 {
            return Err("Attach up to four images at a time.".into());
        }
        if metadata.len() > media::MAX_SOURCE_BYTES as u64 {
            return Err(format!("{name} exceeds the 64 MB image source limit."));
        }
        file.rewind().map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.take(media::MAX_SOURCE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        let image = media::normalize_for_transport(bytes, 2 * 1024 * 1024)
            .map_err(|error| format!("{name}: {error}"))?;
        use base64::Engine;
        attachments.push(Attachment::Image {
            name,
            mime: image.mime.into(),
            data: base64::engine::general_purpose::STANDARD.encode(image.bytes),
            note: image.note,
        });
    }
    Ok(attachments)
}

#[tauri::command]
pub(crate) async fn attachment_choose(kind: Selection) -> Result<Option<Vec<Attachment>>, String> {
    let dialog = rfd::AsyncFileDialog::new();
    let picked = match kind {
        Selection::Files => {
            dialog
                .set_title("Attach files or images")
                .pick_files()
                .await
        }
        Selection::Folder => dialog
            .set_title("Attach a folder")
            .pick_folder()
            .await
            .map(|folder| vec![folder]),
    };
    let Some(picked) = picked else {
        return Ok(None);
    };
    tauri::async_runtime::spawn_blocking(move || {
        selected(
            picked
                .into_iter()
                .map(|file| file.path().to_owned())
                .collect(),
        )
    })
    .await
    .map_err(|error| error.to_string())?
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_and_folders_keep_exact_paths_without_copying_or_parsing_their_contents() {
        let root = test_support::scratch("desktop-attachments");
        let document = root.join("My report.pdf");
        let folder = root.join("My project");
        std::fs::write(&document, b"%PDF-example").unwrap();
        std::fs::create_dir(&folder).unwrap();
        let value = serde_json::to_value(selected(vec![document.clone(), folder.clone()]).unwrap())
            .unwrap();
        assert_eq!(value[0]["kind"], "file");
        assert_eq!(value[0]["path"], document.to_str().unwrap());
        assert_eq!(value[1]["kind"], "folder");
        assert_eq!(value[1]["path"], folder.to_str().unwrap());
        assert!(value[0].get("data").is_none());
    }

    #[test]
    fn images_are_identified_by_bytes_and_bad_images_and_excess_sources_are_refused() {
        use base64::Engine;
        let root = test_support::scratch("desktop-image-attachment");
        let png = root.join("Screenshot.dat");
        let data = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";
        std::fs::write(
            &png,
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
        )
        .unwrap();
        let value = serde_json::to_value(selected(vec![png.clone()]).unwrap()).unwrap();
        assert_eq!(value[0]["kind"], "image");
        assert_eq!(value[0]["name"], "Screenshot.dat");
        assert_eq!(value[0]["mime"], "image/png");
        assert!(selected(vec![png.clone(); 5]).is_err());
        assert!(selected(vec![png.clone(); 17]).is_err());
        let unsupported = root.join("image.qoi");
        let qoi_header = b"qoif\0\0\0\x01\0\0\0\x01\x03\0";
        assert!(media::has_image_header(qoi_header));
        std::fs::write(&unsupported, qoi_header).unwrap();
        let reference = serde_json::to_value(selected(vec![unsupported.clone()]).unwrap()).unwrap();
        assert_eq!(reference[0]["kind"], "file");
        assert_eq!(reference[0]["path"], unsupported.to_str().unwrap());
        std::fs::write(&png, b"\x89PNG\r\n\x1a\ninvalid").unwrap();
        assert!(selected(vec![png]).is_err());
    }
}
