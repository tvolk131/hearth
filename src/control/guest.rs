use anyhow::{Result, ensure};
use serde_json::Value;

pub fn public_key_file(contents: &str) -> Result<String> {
    crate::ssh_key::key_identity(contents.trim_end())
}

pub fn address(vm_id: u16) -> Result<String> {
    ensure!(
        (9001..=9099).contains(&vm_id),
        "guest VMID is outside the disposable lab range"
    );
    Ok(format!("10.77.0.{}", vm_id - 9000 + 10))
}

pub fn encode_key(key: &str) -> String {
    key.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

pub fn image_metadata(value: &Value, disk_mib: u64) -> Result<()> {
    ensure!(
        value["format"] == "qcow2",
        "only pinned qcow2 images are supported"
    );
    let size = value["virtual-size"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("image virtual size is missing"))?;
    ensure!(
        size > 0 && size <= disk_mib * 1024 * 1024,
        "image does not fit the reserved root disk"
    );
    fn standalone(value: &Value) -> bool {
        match value {
            Value::Object(fields) => fields.iter().all(|(key, value)| {
                (!matches!(
                    key.as_str(),
                    "backing-filename" | "full-backing-filename" | "data-file"
                ) || value.as_str() == Some(""))
                    && (key != "encrypted" || value == &Value::Bool(false))
                    && (key != "corrupt" || value == &Value::Bool(false))
                    && standalone(value)
            }),
            Value::Array(values) => values.iter().all(standalone),
            _ => true,
        }
    }
    ensure!(
        standalone(value),
        "image has external storage, encryption or corruption flags"
    );
    Ok(())
}
