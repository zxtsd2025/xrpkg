//! RPM `%caps` 文本到 Linux VFS `security.capability` 二进制的转换。
//!
//! RPM 头里的 `%caps` 是文本形式，例如 `cap_net_raw=ep`、`cap_chown,cap_setuid=ep`
//! 或 `all=ep`。内核 xattr 需要 `struct vfs_cap_data` 的小端二进制，本模块
//! 负责解析、按顺序应用集合操作并编码。`rpm` crate 只校验文本，不提供位编码。
//!
//! 语法：空白分隔的子句，每句 `capset op perms`；`capset` 是逗号分隔的
//! `cap_*` 或 `all`，`op` 为 `=`/`+`/`-`，`perms` 为 `e`/`i`/`p` 的组合。

/// `struct vfs_cap_data` 的 REVISION_2 魔数。
pub const VFS_CAP_REVISION_2: u32 = 0x0200_0000;
/// 带 rootid 的 REVISION_3 魔数（capability 位号 > 31 时使用）。
pub const VFS_CAP_REVISION_3: u32 = 0x0300_0000;
/// `magic_etc` 中的 effective 标志。
pub const VFS_CAP_FLAGS_EFFECTIVE: u32 = 0x0000_0001;

/// 当前 Linux 最大的 capability 位号（`CAP_CHECKPOINT_RESTORE`）。
const MAX_CAPABILITY: u32 = 40;

/// 按 `linux/capability.h` 顺序排列的 capability 名称（位号 0..=40）。
const CAPABILITY_NAMES: [&str; 41] = [
    "cap_chown",
    "cap_dac_override",
    "cap_dac_read_search",
    "cap_fowner",
    "cap_fsetid",
    "cap_kill",
    "cap_setgid",
    "cap_setuid",
    "cap_setpcap",
    "cap_linux_immutable",
    "cap_net_bind_service",
    "cap_net_broadcast",
    "cap_net_admin",
    "cap_net_raw",
    "cap_ipc_lock",
    "cap_ipc_owner",
    "cap_sys_module",
    "cap_sys_rawio",
    "cap_sys_chroot",
    "cap_sys_ptrace",
    "cap_sys_pacct",
    "cap_sys_admin",
    "cap_sys_boot",
    "cap_sys_nice",
    "cap_sys_resource",
    "cap_sys_time",
    "cap_sys_tty_config",
    "cap_mknod",
    "cap_lease",
    "cap_audit_write",
    "cap_audit_control",
    "cap_setfcap",
    "cap_mac_override",
    "cap_mac_admin",
    "cap_syslog",
    "cap_wake_alarm",
    "cap_block_suspend",
    "cap_audit_read",
    "cap_perfmon",
    "cap_bpf",
    "cap_checkpoint_restore",
];

/// 三个 capability 集合的当前状态；位号用 u64 保存以覆盖 0..=40。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CapabilitySets {
    permitted: u64,
    inheritable: u64,
    effective: bool,
}

/// 把 RPM `%caps` 文本编码成 `security.capability` 的字节串。
pub fn encode(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("%caps 为空".to_string());
    }

    let mut sets = CapabilitySets::default();
    for clause in text.split_whitespace() {
        apply_clause(&mut sets, clause)?;
    }
    encode_sets(sets)
}

/// 应用一个 `capset op perms` 子句。
fn apply_clause(sets: &mut CapabilitySets, clause: &str) -> Result<(), String> {
    let position = clause
        .char_indices()
        .find(|(_, character)| matches!(character, '=' | '+' | '-'))
        .map(|(position, _)| position)
        .ok_or_else(|| format!("%caps 子句 `{clause}` 缺少 =/+/- 操作符"))?;
    let (capset, rest) = clause.split_at(position);
    let mut characters = rest.chars();
    let operator = characters.next().expect("操作符位置来自 char_indices");
    let perms = characters.as_str().to_ascii_lowercase();

    if capset.is_empty() {
        return Err(format!("%caps 子句 `{clause}` 缺少 capability 名称"));
    }
    for perm in perms.chars() {
        if !matches!(perm, 'e' | 'i' | 'p') {
            return Err(format!(
                "%caps 子句 `{clause}` 的权限位 `{perm}` 非法；只允许 e/i/p"
            ));
        }
    }

    let bits = parse_capset(capset)?;
    match operator {
        '=' => {
            sets.permitted = 0;
            sets.inheritable = 0;
            sets.effective = false;
        }
        '+' | '-' => {}
        other => {
            return Err(format!(
                "%caps 子句 `{clause}` 的操作符 `{other}` 非法；只允许 =/+/-"
            ))
        }
    }

    let add = operator != '-';
    if perms.contains('p') {
        apply_bits(&mut sets.permitted, bits, add);
    }
    if perms.contains('i') {
        apply_bits(&mut sets.inheritable, bits, add);
    }
    if perms.contains('e') {
        sets.effective = add;
    }
    Ok(())
}

/// 解析 capability 名集合；`all` 展开为 0..=40。
fn parse_capset(capset: &str) -> Result<u64, String> {
    let mut bits = 0;
    for name in capset.split(',') {
        let name = name.trim();
        if name.is_empty() {
            return Err(format!("%caps 的 capability 集合 `{capset}` 含有空项"));
        }
        let lowered = name.to_ascii_lowercase();
        if lowered == "all" {
            for capability in 0..=MAX_CAPABILITY {
                bits |= 1u64 << capability;
            }
            continue;
        }
        let index = CAPABILITY_NAMES
            .iter()
            .position(|candidate| *candidate == lowered)
            .ok_or_else(|| format!("未知 capability `{name}`；期望 cap_* 或 all"))?;
        bits |= 1u64 << index;
    }
    Ok(bits)
}

/// 按 `add` 设置或清除位。
fn apply_bits(target: &mut u64, bits: u64, add: bool) {
    if add {
        *target |= bits;
    } else {
        *target &= !bits;
    }
}

/// 编码 `struct vfs_cap_data` / `struct vfs_ns_cap_data`。
fn encode_sets(sets: CapabilitySets) -> Result<Vec<u8>, String> {
    let high = (sets.permitted >> 32) | (sets.inheritable >> 32);
    let revision = if high != 0 {
        VFS_CAP_REVISION_3
    } else {
        VFS_CAP_REVISION_2
    };
    let magic = revision
        | if sets.effective {
            VFS_CAP_FLAGS_EFFECTIVE
        } else {
            0
        };

    let mut output = Vec::with_capacity(if revision == VFS_CAP_REVISION_3 {
        24
    } else {
        20
    });
    output.extend_from_slice(&magic.to_le_bytes());
    // struct vfs_cap_data.data[0] = { permitted 低 32 位, inheritable 低 32 位 }
    output.extend_from_slice(&(sets.permitted as u32).to_le_bytes());
    output.extend_from_slice(&(sets.inheritable as u32).to_le_bytes());
    // data[1] = { permitted 高 32 位, inheritable 高 32 位 }
    output.extend_from_slice(&((sets.permitted >> 32) as u32).to_le_bytes());
    output.extend_from_slice(&((sets.inheritable >> 32) as u32).to_le_bytes());
    if revision == VFS_CAP_REVISION_3 {
        // rootid = 0：文件属于初始 user namespace。
        output.extend_from_slice(&0u32.to_le_bytes());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 小端字节的 32 位字序列，便于阅读期望值。
    fn words(blob: &[u8]) -> Vec<u32> {
        blob.as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_le_bytes(*chunk))
            .collect()
    }

    #[test]
    fn net_raw_effective_and_permitted_is_revision_2() {
        let blob = encode("cap_net_raw=ep").unwrap();
        assert_eq!(blob.len(), 20);
        assert_eq!(
            words(&blob),
            vec![
                VFS_CAP_REVISION_2 | VFS_CAP_FLAGS_EFFECTIVE,
                1 << 13, // cap_net_raw = 13
                0,
                0,
                0,
            ]
        );
    }

    #[test]
    fn multiple_caps_share_one_clause() {
        let blob = encode("cap_chown,cap_setuid=ep").unwrap();
        assert_eq!(words(&blob)[1], (1 << 0) | (1 << 7));
        assert_eq!(words(&blob)[2], 0);
    }

    #[test]
    fn all_expands_to_revision_3() {
        let blob = encode("all=ep").unwrap();
        assert_eq!(blob.len(), 24);
        let words = words(&blob);
        assert_eq!(words[0], VFS_CAP_REVISION_3 | VFS_CAP_FLAGS_EFFECTIVE);
        assert_eq!(words[1], u32::MAX);
        assert_eq!(words[2], 0);
        assert_eq!(words[3], 0x1ff);
        assert_eq!(words[4], 0);
        assert_eq!(words[5], 0); // rootid
    }

    #[test]
    fn names_are_case_insensitive() {
        assert_eq!(
            encode("CAP_NET_RAW=EP").unwrap(),
            encode("cap_net_raw=ep").unwrap()
        );
    }

    #[test]
    fn inheritable_bit_is_encoded() {
        let blob = encode("cap_net_raw+ip").unwrap();
        let words = words(&blob);
        assert_eq!(words[0], VFS_CAP_REVISION_2);
        assert_eq!(words[1], 1 << 13);
        assert_eq!(words[2], 1 << 13);
    }

    #[test]
    fn minus_removes_bits_and_effective_flag() {
        let blob = encode("cap_net_raw=ep cap_net_raw-ep").unwrap();
        let words = words(&blob);
        assert_eq!(words[0], VFS_CAP_REVISION_2);
        assert_eq!(words[1], 0);
    }

    #[test]
    fn invalid_names_and_operators_are_rejected() {
        for text in [
            "cap_not_real=ep",
            "cap_net_raw",
            "cap_net_raw?pe",
            "cap_net_raw=ex",
            ",cap_net_raw=ep",
            "cap_net_raw,=ep",
        ] {
            assert!(encode(text).is_err(), "本应拒绝: {text:?}");
        }
    }
}
