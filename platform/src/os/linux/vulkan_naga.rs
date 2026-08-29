use {
    crate::makepad_script::{
        shader::ShaderOutput, shader_wgsl::compile_draw_shader_wgsl_source, value::ScriptObject,
        vm::ScriptVm,
    },
    crate::shader_compile::{shader_cache_dir, shader_cache_key},
    std::fmt::Write,
};

// Bump when the naga version, WGSL generation, or the on-disk SPIR-V encoding
// changes, so stale `.spv` blobs from an older engine are invalidated instead
// of being fed to the driver as mismatched bytes. SPIR-V is driver-independent,
// so unlike Metal/Vulkan-pipeline blobs this is the ONLY invalidation lever —
// no device fingerprint is needed in the key.
const VULKAN_CACHE_KEY_VERSION: u8 = 1;

const SPIRV_MAGIC: u32 = 0x0723_0203;

// A SPIR-V blob is a stream of little-endian u32 words beginning with the magic
// number. A file that fails either check is treated as a cache miss (truncated
// write, wrong-endian, or foreign format) and recompiled — never handed to the
// driver.
fn spirv_bytes_to_words(bytes: &[u8]) -> Option<Vec<u32>> {
    if bytes.len() < 4 || bytes.len() % 4 != 0 {
        return None;
    }
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if *words.first()? != SPIRV_MAGIC {
        return None;
    }
    Some(words)
}

fn spirv_words_to_bytes(words: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(words.len() * 4);
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

#[derive(Clone)]
pub struct CxVulkanShaderBinary {
    pub vertex_spirv: Option<Vec<u32>>,
    pub fragment_spirv: Option<Vec<u32>>,
    pub dyn_uniform_binding: u32,
    pub texture_binding_base: u32,
    pub sampler_binding_base: u32,
    pub xr_depth_binding: u32,
    pub geometry_slots: usize,
    pub instance_slots: usize,
}

fn compile_wgsl_to_spirv(wgsl: &str) -> Result<(Option<Vec<u32>>, Option<Vec<u32>>), String> {
    use naga::{back::spv, valid};

    fn extract_error_line(details: &str) -> Option<usize> {
        let marker = "wgsl:";
        let start = details.find(marker)? + marker.len();
        let rest = &details[start..];
        let end = rest.find(':')?;
        rest[..end].trim().parse::<usize>().ok()
    }

    fn wgsl_context(wgsl: &str, line: usize, radius: usize) -> String {
        let start = line.saturating_sub(radius).max(1);
        let end = line.saturating_add(radius);
        let mut out = String::new();
        for (i, src_line) in wgsl.lines().enumerate() {
            let ln = i + 1;
            if ln >= start && ln <= end {
                let _ = writeln!(out, "{ln:4} | {src_line}");
            }
        }
        out
    }

    let module = naga::front::wgsl::parse_str(wgsl).map_err(|e| {
        let details = e.emit_to_string(wgsl);
        let context = extract_error_line(&details)
            .map(|line| {
                format!(
                    "\nWGSL context around line {line}:\n{}",
                    wgsl_context(wgsl, line, 4)
                )
            })
            .unwrap_or_default();
        format!("WGSL parse error: {e}\n{details}{context}")
    })?;

    let mut validator =
        valid::Validator::new(valid::ValidationFlags::all(), valid::Capabilities::all());
    let module_info = validator
        .validate(&module)
        .map_err(|e| format!("WGSL validation error: {e}"))?;

    let options = spv::Options {
        lang_version: (1, 3),
        flags: spv::WriterFlags::empty(),
        fake_missing_bindings: true,
        binding_map: spv::BindingMap::default(),
        capabilities: None,
        bounds_check_policies: naga::proc::BoundsCheckPolicies::default(),
        zero_initialize_workgroup_memory: spv::ZeroInitializeWorkgroupMemoryMode::None,
        force_loop_bounding: false,
        use_storage_input_output_16: false,
        debug_info: None,
    };

    let has_vertex = module
        .entry_points
        .iter()
        .any(|ep| ep.stage == naga::ShaderStage::Vertex && ep.name == "vertex_main");
    let has_fragment = module
        .entry_points
        .iter()
        .any(|ep| ep.stage == naga::ShaderStage::Fragment && ep.name == "fragment_main");

    if !has_vertex && !has_fragment {
        return Err("WGSL module has no entry points".to_string());
    }

    let vertex_spirv = if has_vertex {
        let pipeline = spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Vertex,
            entry_point: "vertex_main".to_string(),
        };
        Some(
            spv::write_vec(&module, &module_info, &options, Some(&pipeline))
                .map_err(|e| format!("SPIR-V write failed for vertex_main: {e}"))?,
        )
    } else {
        None
    };

    let fragment_spirv = if has_fragment {
        let pipeline = spv::PipelineOptions {
            shader_stage: naga::ShaderStage::Fragment,
            entry_point: "fragment_main".to_string(),
        };
        Some(
            spv::write_vec(&module, &module_info, &options, Some(&pipeline))
                .map_err(|e| format!("SPIR-V write failed for fragment_main: {e}"))?,
        )
    } else {
        None
    };

    Ok((vertex_spirv, fragment_spirv))
}

// Read-or-create around the naga compile: on a warm launch the SPIR-V is read
// straight off disk and naga is never invoked; on a cold launch (or a stale/
// corrupt cache entry) it compiles and writes both stages back. Mirrors the
// D3D11 `get_or_compile_shader_bytes` pattern (os/windows/d3d11.rs), but keyed
// off the shared `shader_compile` helpers so all backends share one cache root.
//
// Under MAKEPAD_SHADER_BENCH it prints a HIT/MISS line per shader with the
// cache key and naga-compile ms, so a cold-then-warm relaunch shows the
// timing collapse that is the acceptance test for this change.
fn compile_wgsl_to_spirv_cached(
    wgsl: &str,
) -> Result<(Option<Vec<u32>>, Option<Vec<u32>>), String> {
    let bench = std::env::var_os("MAKEPAD_SHADER_BENCH").is_some();
    let key = shader_cache_key(wgsl, VULKAN_CACHE_KEY_VERSION);
    let dir = shader_cache_dir("vulkan_spirv");

    // Fast path: both stages present and valid on disk => HIT, skip naga.
    if let Some(dir) = &dir {
        let vs_path = dir.join(format!("{:016x}_vs.spv", key));
        let fs_path = dir.join(format!("{:016x}_fs.spv", key));
        // A shader can legitimately have only one stage; a stage is "cached"
        // when its file is absent-by-design or present-and-valid. We record
        // presence so a missing-because-none-exists stage still counts as a
        // hit, while a missing-because-not-yet-written stage forces a miss.
        let vs = std::fs::read(&vs_path).ok().and_then(|b| spirv_bytes_to_words(&b));
        let fs = std::fs::read(&fs_path).ok().and_then(|b| spirv_bytes_to_words(&b));
        // Only treat as a hit when at least one stage read back cleanly and no
        // present-but-corrupt file was seen. If either file exists but failed
        // validation, fall through to recompile (which overwrites it).
        let vs_ok = !vs_path.exists() || vs.is_some();
        let fs_ok = !fs_path.exists() || fs.is_some();
        let any_present = vs.is_some() || fs.is_some();
        if any_present && vs_ok && fs_ok {
            if bench {
                crate::log!(
                    "MPSHADERBENCH vulkan HIT key={:016x} vs={} fs={}",
                    key,
                    vs.is_some(),
                    fs.is_some()
                );
            }
            return Ok((vs, fs));
        }
    }

    // Miss: compile with naga, then write both stages back to disk.
    let t0 = std::time::Instant::now();
    let (vertex_spirv, fragment_spirv) = compile_wgsl_to_spirv(wgsl)?;
    let compile_ms = t0.elapsed().as_secs_f64() * 1000.0;

    if let Some(dir) = &dir {
        if let Some(words) = &vertex_spirv {
            let _ = std::fs::write(
                dir.join(format!("{:016x}_vs.spv", key)),
                spirv_words_to_bytes(words),
            );
        }
        if let Some(words) = &fragment_spirv {
            let _ = std::fs::write(
                dir.join(format!("{:016x}_fs.spv", key)),
                spirv_words_to_bytes(words),
            );
        }
    }

    if bench {
        crate::log!(
            "MPSHADERBENCH vulkan MISS key={:016x} naga={:.2}ms vs={} fs={}",
            key,
            compile_ms,
            vertex_spirv.is_some(),
            fragment_spirv.is_some()
        );
    }

    Ok((vertex_spirv, fragment_spirv))
}

pub(crate) fn compile_draw_shader_wgsl_to_spirv(
    vm: &mut ScriptVm,
    io_self: ScriptObject,
    layout_source: &ShaderOutput,
    xr_multiview: bool,
) -> Result<CxVulkanShaderBinary, String> {
    let wgsl_source = compile_draw_shader_wgsl_source(vm, io_self, layout_source, xr_multiview)?;

    if std::env::var_os("MAKEPAD_DUMP_VULKAN_WGSL").is_some() {
        let variant = if xr_multiview { "xr" } else { "window" };
        crate::log!("---- Vulkan WGSL ({variant}) ----\n{}", wgsl_source.wgsl);
    }

    let (vertex_spirv, fragment_spirv) = compile_wgsl_to_spirv_cached(&wgsl_source.wgsl)
        .map_err(|err| format!("{err}\nSet MAKEPAD_DUMP_VULKAN_WGSL=1 to dump generated WGSL."))?;

    Ok(CxVulkanShaderBinary {
        vertex_spirv,
        fragment_spirv,
        dyn_uniform_binding: wgsl_source.dyn_uniform_binding,
        texture_binding_base: wgsl_source.texture_binding_base,
        sampler_binding_base: wgsl_source.sampler_binding_base,
        xr_depth_binding: wgsl_source.xr_depth_binding,
        geometry_slots: wgsl_source.geometry_slots,
        instance_slots: wgsl_source.instance_slots,
    })
}
