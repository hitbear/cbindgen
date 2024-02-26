/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io::{Read, Write};
use std::path;
use std::rc::Rc;

use crate::bindgen::config::{Config, Language};
use crate::bindgen::ir::{
    Constant, Function, ItemContainer, ItemMap, Path as BindgenPath, Static, Struct, Typedef,
};
use crate::bindgen::writer::{Source, SourceWriter};

/// A bindings header that can be written.
pub struct Bindings {
    pub config: Config,
    /// The map from path to struct, used to lookup whether a given type is a
    /// transparent struct. This is needed to generate code for constants.
    struct_map: ItemMap<Struct>,
    typedef_map: ItemMap<Typedef>,
    struct_fileds_memo: RefCell<HashMap<BindgenPath, Rc<Vec<String>>>>,
    globals: Vec<Static>,
    constants: Vec<Constant>,
    items: Vec<ItemContainer>,
    functions: Vec<Function>,
    source_files: Vec<path::PathBuf>,
    /// Bindings are generated by a recursive call to cbindgen
    /// and shouldn't do anything when written anywhere.
    noop: bool,
    package_version: String,
}

#[derive(PartialEq, Eq)]
enum NamespaceOperation {
    Open,
    Close,
}

impl Bindings {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        config: Config,
        struct_map: ItemMap<Struct>,
        typedef_map: ItemMap<Typedef>,
        constants: Vec<Constant>,
        globals: Vec<Static>,
        items: Vec<ItemContainer>,
        functions: Vec<Function>,
        source_files: Vec<path::PathBuf>,
        noop: bool,
        package_version: String,
    ) -> Bindings {
        Bindings {
            config,
            struct_map,
            typedef_map,
            struct_fileds_memo: Default::default(),
            globals,
            constants,
            items,
            functions,
            source_files,
            noop,
            package_version,
        }
    }

    // FIXME(emilio): What to do when the configuration doesn't match?
    pub fn struct_is_transparent(&self, path: &BindgenPath) -> bool {
        let mut any = false;
        self.struct_map.for_items(path, |s| any |= s.is_transparent);
        any
    }

    /// Peels through typedefs to allow resolving structs.
    fn resolved_struct_path<'a>(&self, path: &'a BindgenPath) -> Cow<'a, BindgenPath> {
        use crate::bindgen::ir::Type;

        let mut resolved_path = Cow::Borrowed(path);
        loop {
            let mut found = None;
            self.typedef_map.for_items(&resolved_path, |item| {
                if let Type::Path(ref p) = item.aliased {
                    found = Some(p.path().clone());
                }
            });
            resolved_path = match found {
                Some(p) => Cow::Owned(p),
                None => break,
            }
        }
        resolved_path
    }

    pub fn struct_exists(&self, path: &BindgenPath) -> bool {
        let mut any = false;
        self.struct_map
            .for_items(&self.resolved_struct_path(path), |_| any = true);
        any
    }

    pub fn struct_field_names(&self, path: &BindgenPath) -> Rc<Vec<String>> {
        let mut memos = self.struct_fileds_memo.borrow_mut();
        if let Some(memo) = memos.get(path) {
            return memo.clone();
        }

        let resolved_path = self.resolved_struct_path(path);

        let mut fields = Vec::<String>::new();
        self.struct_map.for_items(&resolved_path, |st| {
            let mut pos: usize = 0;
            for field in &st.fields {
                if let Some(found_pos) = fields.iter().position(|v| *v == field.name) {
                    pos = found_pos + 1;
                } else {
                    fields.insert(pos, field.name.clone());
                    pos += 1;
                }
            }
        });

        let fields = Rc::new(fields);
        memos.insert(path.clone(), fields.clone());
        if let Cow::Owned(p) = resolved_path {
            memos.insert(p, fields.clone());
        }
        fields
    }

    pub fn generate_depfile<P: AsRef<path::Path>>(&self, header_path: P, depfile_path: P) {
        if let Some(dir) = depfile_path.as_ref().parent() {
            if !dir.exists() {
                std::fs::create_dir_all(dir).unwrap()
            }
        }
        let canon_header_path = header_path.as_ref().canonicalize().unwrap();
        let mut canon_source_files: Vec<_> = self
            .source_files
            .iter()
            .chain(self.config.config_path.as_ref())
            .map(|p| p.canonicalize().unwrap())
            .collect();
        // Sorting makes testing easier by ensuring the output is ordered.
        canon_source_files.sort_unstable();

        // When writing the depfile we must escape whitespace in paths to avoid it being interpreted
        // as a seperator.
        // It is not clear how to otherwise _correctly_ replace whitespace in a non-unicode
        // compliant slice, without knowing the encoding, so we lossy convert such cases,
        // to avoid panics.
        let mut depfile = File::create(depfile_path).unwrap();
        write!(
            &mut depfile,
            "{}:",
            canon_header_path.to_string_lossy().replace(' ', "\\ ")
        )
        .expect("Writing header name to depfile failed");
        canon_source_files.into_iter().for_each(|source_file| {
            // Add line-continue and line-break and then indent with 4 spaces.
            // This makes the output more human-readable.
            depfile.write_all(b" \\\n    ").unwrap();
            let escaped_path = source_file.to_string_lossy().replace(' ', "\\ ");
            depfile.write_all(escaped_path.as_bytes()).unwrap();
        });

        writeln!(&mut depfile).unwrap();

        depfile.flush().unwrap();
    }

    pub fn write_to_file<P: AsRef<path::Path>>(&self, path: P) -> bool {
        if self.noop {
            return false;
        }

        // Don't compare files if we've never written this file before
        if !path.as_ref().is_file() {
            if let Some(parent) = path::Path::new(path.as_ref()).parent() {
                fs::create_dir_all(parent).unwrap();
            }
            self.write(File::create(path).unwrap());
            return true;
        }

        let mut new_file_contents = Vec::new();
        self.write(&mut new_file_contents);

        let mut old_file_contents = Vec::new();
        {
            let mut old_file = File::open(&path).unwrap();
            old_file.read_to_end(&mut old_file_contents).unwrap();
        }

        if old_file_contents != new_file_contents {
            let mut new_file = File::create(&path).unwrap();
            new_file.write_all(&new_file_contents).unwrap();
            true
        } else {
            false
        }
    }

    pub fn write_headers<F: Write>(&self, out: &mut SourceWriter<F>) {
        if self.noop {
            return;
        }

        if let Some(ref f) = self.config.header {
            out.new_line_if_not_start();
            write!(out, "{}", f);
            out.new_line();
        }
        if let Some(f) = self.config.include_guard() {
            out.new_line_if_not_start();
            write!(out, "#ifndef {}", f);
            out.new_line();
            write!(out, "#define {}", f);
            out.new_line();
        }
        if self.config.package_version {
            out.new_line_if_not_start();
            write!(
                out,
                "/* Package version: {} */",
                self.package_version,
            );
            out.new_line();
        }
        if self.config.pragma_once && self.config.language != Language::Cython {
            out.new_line_if_not_start();
            write!(out, "#pragma once");
            out.new_line();
        }
        if self.config.include_version {
            out.new_line_if_not_start();
            write!(
                out,
                "/* Generated with cbindgen:{} */",
                crate::bindgen::config::VERSION
            );
            out.new_line();
        }
        if let Some(ref f) = self.config.autogen_warning {
            out.new_line_if_not_start();
            write!(out, "{}", f);
            out.new_line();
        }

        if self.config.no_includes
            && self.config.sys_includes().is_empty()
            && self.config.includes().is_empty()
            && (self.config.cython.cimports.is_empty() || self.config.language != Language::Cython)
            && self.config.after_includes.is_none()
        {
            return;
        }

        out.new_line_if_not_start();

        if !self.config.no_includes {
            match self.config.language {
                Language::C => {
                    out.write("#include <stdarg.h>");
                    out.new_line();
                    out.write("#include <stdbool.h>");
                    out.new_line();
                    if self.config.usize_is_size_t {
                        out.write("#include <stddef.h>");
                        out.new_line();
                    }
                    out.write("#include <stdint.h>");
                    out.new_line();
                    out.write("#include <stdlib.h>");
                    out.new_line();
                }
                Language::Cxx => {
                    out.write("#include <cstdarg>");
                    out.new_line();
                    if self.config.usize_is_size_t {
                        out.write("#include <cstddef>");
                        out.new_line();
                    }
                    out.write("#include <cstdint>");
                    out.new_line();
                    out.write("#include <cstdlib>");
                    out.new_line();
                    out.write("#include <ostream>");
                    out.new_line();
                    out.write("#include <new>");
                    out.new_line();
                    if self.config.enumeration.cast_assert_name.is_none()
                        && (self.config.enumeration.derive_mut_casts
                            || self.config.enumeration.derive_const_casts)
                    {
                        out.write("#include <cassert>");
                        out.new_line();
                    }
                }
                Language::Cython => {
                    out.write(
                        "from libc.stdint cimport int8_t, int16_t, int32_t, int64_t, intptr_t",
                    );
                    out.new_line();
                    out.write(
                        "from libc.stdint cimport uint8_t, uint16_t, uint32_t, uint64_t, uintptr_t",
                    );
                    out.new_line();
                    out.write("cdef extern from *");
                    out.open_brace();
                    out.write("ctypedef bint bool");
                    out.new_line();
                    out.write("ctypedef struct va_list");
                    out.new_line();
                    out.close_brace(false);
                }
            }
        }

        for include in self.config.sys_includes() {
            write!(out, "#include <{}>", include);
            out.new_line();
        }

        for include in self.config.includes() {
            write!(out, "#include \"{}\"", include);
            out.new_line();
        }

        if self.config.language == Language::Cython {
            for (module, names) in &self.config.cython.cimports {
                write!(out, "from {} cimport {}", module, names.join(", "));
                out.new_line();
            }
        }

        if let Some(ref line) = self.config.after_includes {
            write!(out, "{}", line);
            out.new_line();
        }
    }

    pub fn write<F: Write>(&self, file: F) {
        if self.noop {
            return;
        }

        let mut out = SourceWriter::new(file, self);

        self.write_headers(&mut out);

        self.open_namespaces(&mut out);

        for constant in &self.constants {
            if constant.uses_only_primitive_types() {
                out.new_line_if_not_start();
                constant.write(&self.config, &mut out, None);
                out.new_line();
            }
        }

        for item in &self.items {
            if item
                .deref()
                .annotations()
                .bool("no-export")
                .unwrap_or(false)
            {
                continue;
            }

            out.new_line_if_not_start();
            match *item {
                ItemContainer::Constant(..) => unreachable!(),
                ItemContainer::Static(..) => unreachable!(),
                ItemContainer::Enum(ref x) => x.write(&self.config, &mut out),
                ItemContainer::Struct(ref x) => x.write(&self.config, &mut out),
                ItemContainer::Union(ref x) => x.write(&self.config, &mut out),
                ItemContainer::OpaqueItem(ref x) => x.write(&self.config, &mut out),
                ItemContainer::Typedef(ref x) => x.write(&self.config, &mut out),
            }
            out.new_line();
        }

        for constant in &self.constants {
            if !constant.uses_only_primitive_types() {
                out.new_line_if_not_start();
                constant.write(&self.config, &mut out, None);
                out.new_line();
            }
        }

        if !self.functions.is_empty() || !self.globals.is_empty() {
            if self.config.cpp_compatible_c() {
                out.new_line_if_not_start();
                out.write("#ifdef __cplusplus");
            }

            if self.config.language == Language::Cxx {
                if let Some(ref using_namespaces) = self.config.using_namespaces {
                    for namespace in using_namespaces {
                        out.new_line();
                        write!(out, "using namespace {};", namespace);
                    }
                    out.new_line();
                }
            }

            if self.config.language == Language::Cxx || self.config.cpp_compatible_c() {
                out.new_line();
                out.write("extern \"C\" {");
                out.new_line();
            }

            if self.config.cpp_compatible_c() {
                out.write("#endif // __cplusplus");
                out.new_line();
            }

            for global in &self.globals {
                out.new_line_if_not_start();
                global.write(&self.config, &mut out);
                out.new_line();
            }

            for function in &self.functions {
                out.new_line_if_not_start();
                function.write(&self.config, &mut out);
                out.new_line();
            }

            if self.config.cpp_compatible_c() {
                out.new_line();
                out.write("#ifdef __cplusplus");
            }

            if self.config.language == Language::Cxx || self.config.cpp_compatible_c() {
                out.new_line();
                out.write("} // extern \"C\"");
                out.new_line();
            }

            if self.config.cpp_compatible_c() {
                out.write("#endif // __cplusplus");
                out.new_line();
            }
        }

        if self.config.language == Language::Cython
            && self.globals.is_empty()
            && self.constants.is_empty()
            && self.items.is_empty()
            && self.functions.is_empty()
        {
            out.write("pass");
        }

        self.close_namespaces(&mut out);

        if let Some(f) = self.config.include_guard() {
            out.new_line_if_not_start();
            if self.config.language == Language::C {
                write!(out, "#endif /* {} */", f);
            } else {
                write!(out, "#endif // {}", f);
            }
            out.new_line();
        }
        if let Some(ref f) = self.config.trailer {
            out.new_line_if_not_start();
            write!(out, "{}", f);
            if !f.ends_with('\n') {
                out.new_line();
            }
        }
    }

    fn all_namespaces(&self) -> Vec<&str> {
        if self.config.language != Language::Cxx && !self.config.cpp_compatible_c() {
            return vec![];
        }
        let mut ret = vec![];
        if let Some(ref namespace) = self.config.namespace {
            ret.push(&**namespace);
        }
        if let Some(ref namespaces) = self.config.namespaces {
            for namespace in namespaces {
                ret.push(&**namespace);
            }
        }
        ret
    }

    fn open_close_namespaces<F: Write>(&self, op: NamespaceOperation, out: &mut SourceWriter<F>) {
        if self.config.language == Language::Cython {
            if op == NamespaceOperation::Open {
                out.new_line();
                let header = self.config.cython.header.as_deref().unwrap_or("*");
                write!(out, "cdef extern from {}", header);
                out.open_brace();
            } else {
                out.close_brace(false);
            }
            return;
        }

        let mut namespaces = self.all_namespaces();
        if namespaces.is_empty() {
            return;
        }

        if op == NamespaceOperation::Close {
            namespaces.reverse();
        }

        if self.config.cpp_compatible_c() {
            out.new_line_if_not_start();
            out.write("#ifdef __cplusplus");
        }

        for namespace in namespaces {
            out.new_line();
            match op {
                NamespaceOperation::Open => write!(out, "namespace {} {{", namespace),
                NamespaceOperation::Close => write!(out, "}} // namespace {}", namespace),
            }
        }

        out.new_line();
        if self.config.cpp_compatible_c() {
            out.write("#endif // __cplusplus");
            out.new_line();
        }
    }

    pub(crate) fn open_namespaces<F: Write>(&self, out: &mut SourceWriter<F>) {
        self.open_close_namespaces(NamespaceOperation::Open, out);
    }

    pub(crate) fn close_namespaces<F: Write>(&self, out: &mut SourceWriter<F>) {
        self.open_close_namespaces(NamespaceOperation::Close, out);
    }
}
