//! Define the `Resolver` trait, allowing custom resolution for external
//! references.

use crate::{ImportError, LinkError};
use more_asserts::assert_ge;
use wasmer_types::entity::{BoxedSlice, EntityRef, PrimaryMap};
use wasmer_types::{ExternType, FunctionIndex, ImportIndex, MemoryIndex, TableIndex};

use wasmer_vm::{
    Export, FunctionBodyPtr, Imports, MemoryStyle, ModuleInfo, TableStyle, VMFunctionBody,
    VMFunctionImport, VMFunctionKind, VMGlobalImport, VMMemoryImport, VMTableImport,
};

/// Import resolver connects imports with available exported values.
pub trait Resolver {
    /// Resolves an import a WebAssembly module to an export it's hooked up to.
    ///
    /// The `index` provided is the index of the import in the wasm module
    /// that's being resolved. For example 1 means that it's the second import
    /// listed in the wasm module.
    ///
    /// The `module` and `field` arguments provided are the module/field names
    /// listed on the import itself.
    ///
    /// # Notes:
    ///
    /// The index is useful because some WebAssembly modules may rely on that
    /// for resolving ambiguity in their imports. Such as:
    /// ```ignore
    /// (module
    ///   (import "" "" (func))
    ///   (import "" "" (func (param i32) (result i32)))
    /// )
    /// ```
    fn resolve(&self, _index: u32, module: &str, field: &str) -> Option<Export>;
}

/// Import resolver connects imports with available exported values.
///
/// This is a specific subtrait for [`Resolver`] for those users who don't
/// care about the `index`, but only about the `module` and `field` for
/// the resolution.
pub trait NamedResolver {
    /// Resolves an import a WebAssembly module to an export it's hooked up to.
    ///
    /// It receives the `module` and `field` names and return the [`Export`] in
    /// case it's found.
    fn resolve_by_name(&self, module: &str, field: &str) -> Option<Export>;
}

// All NamedResolvers should extend `Resolver`.
impl<T: NamedResolver> Resolver for T {
    /// By default this method will be calling [`NamedResolver::resolve_by_name`],
    /// dismissing the provided `index`.
    fn resolve(&self, _index: u32, module: &str, field: &str) -> Option<Export> {
        self.resolve_by_name(module, field)
    }
}

impl<T: NamedResolver> NamedResolver for &T {
    fn resolve_by_name(&self, module: &str, field: &str) -> Option<Export> {
        (**self).resolve_by_name(module, field)
    }
}

impl NamedResolver for Box<dyn NamedResolver> {
    fn resolve_by_name(&self, module: &str, field: &str) -> Option<Export> {
        (**self).resolve_by_name(module, field)
    }
}

/// `Resolver` implementation that always resolves to `None`.
pub struct NullResolver {}

impl Resolver for NullResolver {
    fn resolve(&self, _idx: u32, _module: &str, _field: &str) -> Option<Export> {
        None
    }
}

/// Get an `ExternType` given a import index.
fn get_extern_from_import(module: &ModuleInfo, import_index: &ImportIndex) -> ExternType {
    match import_index {
        ImportIndex::Function(index) => {
            let func = module.signatures[module.functions[*index]].clone();
            ExternType::Function(func)
        }
        ImportIndex::Table(index) => {
            let table = module.tables[*index];
            ExternType::Table(table)
        }
        ImportIndex::Memory(index) => {
            let memory = module.memories[*index];
            ExternType::Memory(memory)
        }
        ImportIndex::Global(index) => {
            let global = module.globals[*index];
            ExternType::Global(global)
        }
    }
}

/// Get an `ExternType` given an export (and Engine signatures in case is a function).
fn get_extern_from_export(_module: &ModuleInfo, export: &Export) -> ExternType {
    match export {
        Export::Function(ref f) => ExternType::Function(f.signature.clone()),
        Export::Table(ref t) => ExternType::Table(*t.ty()),
        Export::Memory(ref m) => ExternType::Memory(*m.ty()),
        Export::Global(ref g) => {
            let global = g.from.ty();
            ExternType::Global(*global)
        }
    }
}

/// This function allows to match all imports of a `ModuleInfo` with concrete definitions provided by
/// a `Resolver`.
///
/// If all imports are satisfied returns an `Imports` instance required for a module instantiation.
pub fn resolve_imports(
    module: &ModuleInfo,
    resolver: &dyn Resolver,
    finished_dynamic_function_trampolines: &BoxedSlice<FunctionIndex, FunctionBodyPtr>,
    memory_styles: &PrimaryMap<MemoryIndex, MemoryStyle>,
    _table_styles: &PrimaryMap<TableIndex, TableStyle>,
) -> Result<Imports, LinkError> {
    let mut function_imports = PrimaryMap::with_capacity(module.num_imported_functions);
    let mut table_imports = PrimaryMap::with_capacity(module.num_imported_tables);
    let mut memory_imports = PrimaryMap::with_capacity(module.num_imported_memories);
    let mut global_imports = PrimaryMap::with_capacity(module.num_imported_globals);

    for ((module_name, field, import_idx), import_index) in module.imports.iter() {
        let resolved = resolver.resolve(*import_idx, module_name, field);
        let import_extern = get_extern_from_import(module, import_index);
        let resolved = match resolved {
            None => {
                return Err(LinkError::Import(
                    module_name.to_string(),
                    field.to_string(),
                    ImportError::UnknownImport(import_extern),
                ));
            }
            Some(r) => r,
        };
        let export_extern = get_extern_from_export(module, &resolved);
        if !export_extern.is_compatible_with(&import_extern) {
            return Err(LinkError::Import(
                module_name.to_string(),
                field.to_string(),
                ImportError::IncompatibleType(import_extern, export_extern),
            ));
        }
        match resolved {
            Export::Function(ref f) => {
                let address = match f.kind {
                    VMFunctionKind::Dynamic => {
                        // If this is a dynamic imported function,
                        // the address of the function is the address of the
                        // reverse trampoline.
                        let index = FunctionIndex::new(function_imports.len());
                        finished_dynamic_function_trampolines[index].0 as *mut VMFunctionBody as _

                        // TODO: We should check that the f.vmctx actually matches
                        // the shape of `VMDynamicFunctionImportContext`
                    }
                    VMFunctionKind::Static => f.address,
                };
                function_imports.push(VMFunctionImport {
                    body: address,
                    environment: f.vmctx,
                });
            }
            Export::Table(ref t) => {
                table_imports.push(VMTableImport {
                    definition: t.from.vmtable(),
                    from: t.from.clone(),
                });
            }
            Export::Memory(ref m) => {
                match import_index {
                    ImportIndex::Memory(index) => {
                        // Sanity-check: Ensure that the imported memory has at least
                        // guard-page protections the importing module expects it to have.
                        let export_memory_style = m.style();
                        let import_memory_style = &memory_styles[*index];
                        if let (
                            MemoryStyle::Static { bound, .. },
                            MemoryStyle::Static {
                                bound: import_bound,
                                ..
                            },
                        ) = (export_memory_style.clone(), &import_memory_style)
                        {
                            assert_ge!(bound, *import_bound);
                        }
                        assert_ge!(
                            export_memory_style.offset_guard_size(),
                            import_memory_style.offset_guard_size()
                        );
                    }
                    _ => {
                        // This should never be reached, as we did compatibility
                        // checks before
                        panic!("Memory resolution didn't matched");
                    }
                }

                memory_imports.push(VMMemoryImport {
                    definition: m.from.vmmemory(),
                    from: m.from.clone(),
                });
            }

            Export::Global(ref g) => {
                global_imports.push(VMGlobalImport {
                    definition: g.from.vmglobal(),
                    from: g.from.clone(),
                });
            }
        }
    }

    Ok(Imports::new(
        function_imports,
        table_imports,
        memory_imports,
        global_imports,
    ))
}

/// A [`Resolver`] that links two resolvers together in a chain.
pub struct NamedResolverChain<A: NamedResolver, B: NamedResolver> {
    a: A,
    b: B,
}

/// A trait for chaining resolvers together.
///
/// ```
/// # use wasmer_engine::{ChainableNamedResolver, NamedResolver};
/// # fn chainable_test<A, B>(imports1: A, imports2: B)
/// # where A: NamedResolver + Sized,
/// #       B: NamedResolver + Sized,
/// # {
/// // override duplicates with imports from `imports2`
/// imports1.chain_front(imports2);
/// # }
/// ```
pub trait ChainableNamedResolver: NamedResolver + Sized {
    /// Chain a resolver in front of the current resolver.
    ///
    /// This will cause the second resolver to override the first.
    ///
    /// ```
    /// # use wasmer_engine::{ChainableNamedResolver, NamedResolver};
    /// # fn chainable_test<A, B>(imports1: A, imports2: B)
    /// # where A: NamedResolver + Sized,
    /// #       B: NamedResolver + Sized,
    /// # {
    /// // override duplicates with imports from `imports2`
    /// imports1.chain_front(imports2);
    /// # }
    /// ```
    fn chain_front<U>(self, other: U) -> NamedResolverChain<U, Self>
    where
        U: NamedResolver,
    {
        NamedResolverChain { a: other, b: self }
    }

    /// Chain a resolver behind the current resolver.
    ///
    /// This will cause the first resolver to override the second.
    ///
    /// ```
    /// # use wasmer_engine::{ChainableNamedResolver, NamedResolver};
    /// # fn chainable_test<A, B>(imports1: A, imports2: B)
    /// # where A: NamedResolver + Sized,
    /// #       B: NamedResolver + Sized,
    /// # {
    /// // override duplicates with imports from `imports1`
    /// imports1.chain_back(imports2);
    /// # }
    /// ```
    fn chain_back<U>(self, other: U) -> NamedResolverChain<Self, U>
    where
        U: NamedResolver,
    {
        NamedResolverChain { a: self, b: other }
    }
}

// We give these chain methods to all types implementing NamedResolver
impl<T: NamedResolver> ChainableNamedResolver for T {}

impl<A, B> NamedResolver for NamedResolverChain<A, B>
where
    A: NamedResolver,
    B: NamedResolver,
{
    fn resolve_by_name(&self, module: &str, field: &str) -> Option<Export> {
        self.a
            .resolve_by_name(module, field)
            .or_else(|| self.b.resolve_by_name(module, field))
    }
}

impl<A, B> Clone for NamedResolverChain<A, B>
where
    A: NamedResolver + Clone,
    B: NamedResolver + Clone,
{
    fn clone(&self) -> Self {
        Self {
            a: self.a.clone(),
            b: self.b.clone(),
        }
    }
}
