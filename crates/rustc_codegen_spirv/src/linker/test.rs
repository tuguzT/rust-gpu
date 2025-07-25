use super::{LinkResult, link};
use rspirv::dr::{Loader, Module};
use rustc_errors::registry::Registry;
use rustc_session::CompilerIO;
use rustc_session::config::{Input, OutputFilenames, OutputTypes};
use rustc_span::FileName;
use std::io::Write;
use std::sync::{Arc, Mutex};

// `termcolor` is needed because we cannot construct an Emitter after it was added in
// https://github.com/rust-lang/rust/pull/114104. This can be removed when
// https://github.com/rust-lang/rust/pull/115393 lands.
// We need to construct an emitter as yet another workaround,
// see https://github.com/rust-lang/rust/pull/102992.
extern crate termcolor;
use termcolor::{ColorSpec, WriteColor};

// https://github.com/colin-kiegel/rust-pretty-assertions/issues/24
#[derive(PartialEq, Eq)]
struct PrettyString(String);
impl std::fmt::Debug for PrettyString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // HACK(eddyb) add extra newlines for readability when it shows up
        // in `Result::unwrap` panic messages specifically.
        f.write_str("\n")?;
        f.write_str(self)?;
        f.write_str("\n")
    }
}
impl std::ops::Deref for PrettyString {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

fn assemble_spirv(spirv: &str) -> Vec<u8> {
    use spirv_tools::assembler::{self, Assembler};

    let assembler = assembler::create(None);

    let spv_binary = assembler
        .assemble(spirv, assembler::AssemblerOptions::default())
        .expect("Failed to assemble test spir-v");

    let contents: &[u8] = spv_binary.as_ref();
    contents.to_vec()
}

#[allow(unused)]
fn validate(spirv: &[u32]) {
    use spirv_tools::val::{self, Validator};

    let validator = val::create(None);

    validator
        .validate(spirv, None)
        .expect("validation error occurred");
}

fn load(bytes: &[u8]) -> Module {
    let mut loader = Loader::new();
    rspirv::binary::parse_bytes(bytes, &mut loader).unwrap();
    loader.module()
}

// FIXME(eddyb) shouldn't this be named just `link`? (`assemble_spirv` is separate)
fn assemble_and_link(binaries: &[&[u8]]) -> Result<Module, PrettyString> {
    link_with_linker_opts(
        binaries,
        &crate::linker::Options {
            compact_ids: true,
            dce: true,
            keep_link_exports: true,
            ..Default::default()
        },
    )
}

fn link_with_linker_opts(
    binaries: &[&[u8]],
    opts: &crate::linker::Options,
) -> Result<Module, PrettyString> {
    let modules = binaries.iter().cloned().map(load).collect::<Vec<_>>();

    // A threadsafe buffer for writing.
    #[derive(Default, Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl BufWriter {
        fn unwrap_to_string(self) -> String {
            String::from_utf8(Arc::try_unwrap(self.0).ok().unwrap().into_inner().unwrap()).unwrap()
        }
    }

    impl Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }
    impl WriteColor for BufWriter {
        fn supports_color(&self) -> bool {
            false
        }

        fn set_color(&mut self, _spec: &ColorSpec) -> std::io::Result<()> {
            Ok(())
        }

        fn reset(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = BufWriter::default();
    let output = buf.clone();

    // NOTE(eddyb) without `catch_fatal_errors`, you'd get the really strange
    // effect of test failures with no output (because the `FatalError` "panic"
    // is really a silent unwinding device, that should be treated the same as
    // `Err(ErrorGuaranteed)` returns from `link`).
    rustc_driver::catch_fatal_errors(|| {
        rustc_data_structures::jobserver::initialize_checked(|err| {
            unreachable!("jobserver error: {err}");
        });

        let mut early_dcx =
            rustc_session::EarlyDiagCtxt::new(rustc_session::config::ErrorOutputType::default());
        let matches =
            rustc_driver::handle_options(&early_dcx, &["".to_string(), "x.rs".to_string()])
                .unwrap();
        let sopts = rustc_session::config::build_session_options(&mut early_dcx, &matches);

        let target = "spirv-unknown-spv1.0"
            .parse::<crate::target::SpirvTarget>()
            .unwrap()
            .rustc_target();
        let sm_inputs = rustc_span::source_map::SourceMapInputs {
            file_loader: Box::new(rustc_span::source_map::RealFileLoader),
            path_mapping: sopts.file_path_mapping(),
            hash_kind: sopts.unstable_opts.src_hash_algorithm(&target),
            checksum_hash_kind: None,
        };
        rustc_span::create_session_globals_then(sopts.edition, &[], Some(sm_inputs), || {
            extern crate rustc_driver_impl;

            let mut sess = rustc_session::build_session(
                sopts,
                CompilerIO {
                    input: Input::Str {
                        name: FileName::Custom(String::new()),
                        input: String::new(),
                    },
                    output_dir: None,
                    output_file: None,
                    temps_dir: None,
                },
                Default::default(),
                Registry::new(&[]),
                Default::default(),
                Default::default(),
                target,
                Default::default(),
                rustc_interface::util::rustc_version_str().unwrap_or("unknown"),
                Default::default(),
                &rustc_driver_impl::USING_INTERNAL_FEATURES,
                Default::default(),
            );

            // HACK(eddyb) inject `write_diags` into `sess`, to work around
            // the removals in https://github.com/rust-lang/rust/pull/102992.
            sess.psess = {
                let source_map = sess.psess.clone_source_map();

                let emitter = rustc_errors::emitter::HumanEmitter::new(
                    Box::new(buf),
                    rustc_driver_impl::default_translator(),
                )
                .sm(Some(source_map.clone()));

                rustc_session::parse::ParseSess::with_dcx(
                    rustc_errors::DiagCtxt::new(Box::new(emitter))
                        .with_flags(sess.opts.unstable_opts.dcx_flags(true)),
                    source_map,
                )
            };

            let res = link(
                &sess,
                modules,
                opts,
                &OutputFilenames::new(
                    "".into(),
                    "".into(),
                    "".into(),
                    None,
                    None,
                    "".into(),
                    OutputTypes::new(&[]),
                ),
                Default::default(),
            );
            assert_eq!(sess.dcx().has_errors(), res.as_ref().err().copied());
            res.map(|res| match res {
                LinkResult::SingleModule(m) => *m,
                LinkResult::MultipleModules { .. } => unreachable!(),
            })
            .map_err(|_guar| ())
        })
    })
    .map_err(|_fatal| ())
    .flatten()
    .map_err(|()| {
        let mut diags = output.unwrap_to_string();
        if let Some(diags_without_trailing_newlines) = diags.strip_suffix("\n\n") {
            diags.truncate(diags_without_trailing_newlines.len());
        }
        diags
    })
    .map_err(PrettyString)
}

#[track_caller]
fn without_header_eq(output: Module, expected: &str) {
    let result = {
        let disasm = |mut result: Module| {
            use rspirv::binary::Disassemble;
            //use rspirv::binary::Assemble;

            // validate(&result.assemble());

            result.header = None;
            result.disassemble()
        };
        disasm(output)
    };

    let expected = expected
        .split('\n')
        .map(|l| l.trim())
        .collect::<Vec<_>>()
        .join("\n");

    let result = result
        .split('\n')
        .map(|l| l.trim().replace("  ", " ")) // rspirv outputs multiple spaces between operands
        .collect::<Vec<_>>()
        .join("\n");

    if result != expected {
        println!("{}", &result);
        panic!(
            "assertion failed: `left == right`\
            \n\
            \n{}\
            \n",
            pretty_assertions::Comparison::new(&PrettyString(expected), &PrettyString(result))
        );
    }
}

#[test]
fn standard() {
    // FIXME(eddyb) the `Input` `OpVariable` is completely unused and after
    // enabling DCE, it started being removed, is it necessary at all?
    let a = assemble_spirv(
        r#"OpCapability Linkage
        OpMemoryModel Logical OpenCL
        OpDecorate %1 LinkageAttributes "foo" Import
        %2 = OpTypeFloat 32
        %1 = OpVariable %2 Uniform
        %3 = OpVariable %2 Input"#,
    );

    let b = assemble_spirv(
        r#"OpCapability Linkage
        OpMemoryModel Logical OpenCL
        OpDecorate %1 LinkageAttributes "foo" Export
        %2 = OpTypeFloat 32
        %3 = OpConstant %2 42
        %1 = OpVariable %2 Uniform %3
        "#,
    );

    let result = assemble_and_link(&[&a, &b]).unwrap();
    let expect = r#"OpCapability Linkage
        OpMemoryModel Logical OpenCL
        OpDecorate %1 LinkageAttributes "foo" Export
        %2 = OpTypeFloat 32
        %3 = OpConstant %2 42
        %1 = OpVariable %2 Uniform %3"#;

    without_header_eq(result, expect);
}

#[test]
fn not_a_lib_extra_exports() {
    let a = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeFloat 32
            %1 = OpVariable %2 Uniform"#,
    );

    let result = assemble_and_link(&[&a]).unwrap();
    let expect = r#"OpCapability Linkage
        OpMemoryModel Logical OpenCL
        OpDecorate %1 LinkageAttributes "foo" Export
        %2 = OpTypeFloat 32
        %1 = OpVariable %2 Uniform"#;
    without_header_eq(result, expect);
}

#[test]
fn unresolved_symbol() {
    let a = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Import
            %2 = OpTypeFloat 32
            %1 = OpVariable %2 Uniform"#,
    );

    let b = assemble_spirv(
        "OpCapability Linkage
        OpMemoryModel Logical OpenCL",
    );

    let result = assemble_and_link(&[&a, &b]);

    assert_eq!(
        result.err().as_deref(),
        Some("error: Unresolved symbol \"foo\"")
    );
}

#[test]
fn type_mismatch() {
    let a = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Import
            %2 = OpTypeFloat 32
            %1 = OpVariable %2 Uniform
            %3 = OpVariable %2 Input"#,
    );

    let b = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeInt 32 0
            %3 = OpConstant %2 42
            %1 = OpVariable %2 Uniform %3
        "#,
    );

    let result = assemble_and_link(&[&a, &b]);
    assert_eq!(
        result.err().as_deref(),
        Some(
            "error: Types mismatch for \"foo\"\n  |\n  = note: import type: (TypeFloat)\n  = note: export type: (TypeInt)"
        )
    );
}

#[test]
fn multiple_definitions() {
    let a = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Import
            %2 = OpTypeFloat 32
            %1 = OpVariable %2 Uniform
            %3 = OpVariable %2 Input"#,
    );

    let b = assemble_spirv(
        r#"OpCapability Linkage
            OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeFloat 32
            %3 = OpConstant %2 42
            %1 = OpVariable %2 Uniform %3"#,
    );

    let c = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeFloat 32
            %3 = OpConstant %2 -1
            %1 = OpVariable %2 Uniform %3"#,
    );

    let result = assemble_and_link(&[&a, &b, &c]);
    assert_eq!(
        result.err().as_deref(),
        Some("error: Multiple exports found for \"foo\"")
    );
}

#[test]
fn multiple_definitions_different_types() {
    let a = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Import
            %2 = OpTypeFloat 32
            %1 = OpVariable %2 Uniform
            %3 = OpVariable %2 Input"#,
    );

    let b = assemble_spirv(
        r#"OpCapability Linkage
            OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeInt 32 0
            %3 = OpConstant %2 42
            %1 = OpVariable %2 Uniform %3"#,
    );

    let c = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeFloat 32
            %3 = OpConstant %2 12
            %1 = OpVariable %2 Uniform %3"#,
    );

    let result = assemble_and_link(&[&a, &b, &c]);
    assert_eq!(
        result.err().as_deref(),
        Some("error: Multiple exports found for \"foo\"")
    );
}

//jb-todo: this isn't validated yet in the linker (see ensure_matching_import_export_pairs)
/*#[test]
fn decoration_mismatch() {
    let a = assemble_spirv(
        r#"OpCapability Linkage
        OpMemoryModel Logical OpenCL
        OpDecorate %1 LinkageAttributes "foo" Import
        OpDecorate %2 Constant
        %2 = OpTypeFloat 32
        %1 = OpVariable %2 Uniform
        %3 = OpVariable %2 Input"#,
    );

    let b = assemble_spirv(
        r#"OpCapability Linkage
        OpMemoryModel Logical OpenCL
        OpDecorate %1 LinkageAttributes "foo" Export
        %2 = OpTypeFloat 32
        %3 = OpConstant %2 42
        %1 = OpVariable %2 Uniform %3"#,
    );

    let result = assemble_and_link(&[&a, &b]);
    assert_eq!(
        result.err(),
        Some(LinkerError::MultipleExports("foo".to_string()))
    );
    Ok(())
}*/

#[test]
fn func_ctrl() {
    // FIXME(eddyb) the `Uniform` `OpVariable` is completely unused and after
    // enabling DCE, it started being removed, is it necessary at all?
    let a = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Import
            %2 = OpTypeVoid
            %3 = OpTypeFunction %2
            %4 = OpTypeFloat 32
            %5 = OpVariable %4 Uniform
            %1 = OpFunction %2 None %3
            OpFunctionEnd"#,
    );

    let b = assemble_spirv(
        r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeVoid
            %3 = OpTypeFunction %2
            %1 = OpFunction %2 DontInline %3
            %4 = OpLabel
            OpReturn
            OpFunctionEnd"#,
    );

    let result = assemble_and_link(&[&a, &b]).unwrap();

    let expect = r#"OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            %2 = OpTypeVoid
            %3 = OpTypeFunction %2
            %1 = OpFunction %2 DontInline %3
            %4 = OpLabel
            OpReturn
            OpFunctionEnd"#;

    without_header_eq(result, expect);
}

#[test]
fn use_exported_func_param_attr() {
    // HACK(eddyb) this keeps an otherwise-dead `OpFunction` alive w/ an `Export`.
    let a = assemble_spirv(
        r#"OpCapability Kernel
            OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Import
            OpDecorate %3 FuncParamAttr Zext
            OpDecorate %4 FuncParamAttr Zext
            OpDecorate %8 LinkageAttributes "HACK(eddyb) keep function alive" Export
            %5 = OpTypeVoid
            %6 = OpTypeInt 32 0
            %7 = OpTypeFunction %5 %6
            %1 = OpFunction %5 None %7
            %3 = OpFunctionParameter %6
            OpFunctionEnd
            %8 = OpFunction %5 None %7
            %4 = OpFunctionParameter %6
            %9 = OpLabel
            %10 = OpLoad %6 %4
            OpReturn
            OpFunctionEnd
            "#,
    );

    let b = assemble_spirv(
        r#"OpCapability Kernel
            OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpDecorate %1 LinkageAttributes "foo" Export
            OpDecorate %2 FuncParamAttr Sext
            %3 = OpTypeVoid
            %4 = OpTypeInt 32 0
            %5 = OpTypeFunction %3 %4
            %1 = OpFunction %3 None %5
            %2 = OpFunctionParameter %4
            %6 = OpLabel
            %7 = OpLoad %4 %2
            OpReturn
            OpFunctionEnd
            "#,
    );

    let result = assemble_and_link(&[&a, &b]).unwrap();

    let expect = r#"OpCapability Linkage
        OpCapability Kernel
        OpMemoryModel Logical OpenCL
        OpDecorate %1 FuncParamAttr Zext
        OpDecorate %2 FuncParamAttr Sext
        OpDecorate %3 LinkageAttributes "HACK(eddyb) keep function alive" Export
        OpDecorate %4 LinkageAttributes "foo" Export
        %5 = OpTypeVoid
        %6 = OpTypeInt 32 0
        %7 = OpTypeFunction %5 %6
        %3 = OpFunction %5 None %7
        %1 = OpFunctionParameter %6
        %8 = OpLabel
        %9 = OpLoad %6 %1
        OpReturn
        OpFunctionEnd
        %4 = OpFunction %5 None %7
        %2 = OpFunctionParameter %6
        %10 = OpLabel
        %11 = OpLoad %6 %2
        OpReturn
        OpFunctionEnd"#;

    without_header_eq(result, expect);
}

#[test]
fn names_and_decorations() {
    // HACK(eddyb) this keeps an otherwise-dead `OpFunction` alive w/ an `Export`.
    let a = assemble_spirv(
        r#"OpCapability Kernel
            OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpName %1 "foo"
            OpName %3 "param"
            OpDecorate %1 LinkageAttributes "foo" Import
            OpDecorate %3 Restrict
            OpDecorate %4 Restrict
            OpDecorate %4 NonWritable
            OpDecorate %8 LinkageAttributes "HACK(eddyb) keep function alive" Export
            %5 = OpTypeVoid
            %6 = OpTypeInt 32 0
            %9 = OpTypePointer Function %6
            %7 = OpTypeFunction %5 %9
            %1 = OpFunction %5 None %7
            %3 = OpFunctionParameter %9
            OpFunctionEnd
            %8 = OpFunction %5 None %7
            %4 = OpFunctionParameter %9
            %10 = OpLabel
            %11 = OpLoad %6 %4
            OpReturn
            OpFunctionEnd
            "#,
    );

    let b = assemble_spirv(
        r#"OpCapability Kernel
            OpCapability Linkage
            OpMemoryModel Logical OpenCL
            OpName %1 "foo"
            OpName %2 "param"
            OpDecorate %1 LinkageAttributes "foo" Export
            OpDecorate %2 Restrict
            %3 = OpTypeVoid
            %4 = OpTypeInt 32 0
            %7 = OpTypePointer Function %4
            %5 = OpTypeFunction %3 %7
            %1 = OpFunction %3 None %5
            %2 = OpFunctionParameter %7
            %6 = OpLabel
            %8 = OpLoad %4 %2
            OpReturn
            OpFunctionEnd
            "#,
    );

    let result = assemble_and_link(&[&a, &b]).unwrap();

    let expect = r#"OpCapability Linkage
        OpCapability Kernel
        OpMemoryModel Logical OpenCL
        OpName %1 "foo"
        OpName %2 "param"
        OpDecorate %3 Restrict
        OpDecorate %3 NonWritable
        OpDecorate %2 Restrict
        OpDecorate %4 LinkageAttributes "HACK(eddyb) keep function alive" Export
        OpDecorate %1 LinkageAttributes "foo" Export
        %5 = OpTypeVoid
        %6 = OpTypeInt 32 0
        %7 = OpTypePointer Function %6
        %8 = OpTypeFunction %5 %7
        %4 = OpFunction %5 None %8
        %3 = OpFunctionParameter %7
        %9 = OpLabel
        %10 = OpLoad %6 %3
        OpReturn
        OpFunctionEnd
        %1 = OpFunction %5 None %8
        %2 = OpFunctionParameter %7
        %11 = OpLabel
        %12 = OpLoad %6 %2
        OpReturn
        OpFunctionEnd"#;

    without_header_eq(result, expect);
}
