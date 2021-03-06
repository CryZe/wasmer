use crate::{
    backend::Token,
    backing::{ImportBacking, LocalBacking},
    error::{CallError, CallResult, ResolveError, ResolveResult, Result},
    export::{
        Context, Export, ExportIter, FuncPointer, GlobalPointer, MemoryPointer, TablePointer,
    },
    import::{ImportObject, LikeNamespace},
    module::{ExportIndex, Module, ModuleInner},
    types::{
        FuncIndex, FuncSig, GlobalDesc, GlobalIndex, LocalOrImport, Memory, MemoryIndex, Table,
        TableIndex, Value,
    },
    vm,
};
use std::{mem, rc::Rc};

pub(crate) struct InstanceInner {
    #[allow(dead_code)]
    pub(crate) backing: LocalBacking,
    import_backing: ImportBacking,
    vmctx: Box<vm::Ctx>,
}

/// An instantiated WebAssembly module.
///
/// An `Instance` represents a WebAssembly module that
/// has been instantiated with an [`ImportObject`] and is
/// ready to be called.
///
/// [`ImportObject`]: struct.ImportObject.html
pub struct Instance {
    module: Rc<ModuleInner>,
    inner: Box<InstanceInner>,
    #[allow(dead_code)]
    imports: Box<ImportObject>,
}

impl Instance {
    pub(crate) fn new(module: Rc<ModuleInner>, mut imports: Box<ImportObject>) -> Result<Instance> {
        // We need the backing and import_backing to create a vm::Ctx, but we need
        // a vm::Ctx to create a backing and an import_backing. The solution is to create an
        // uninitialized vm::Ctx and then initialize it in-place.
        let mut vmctx = unsafe { Box::new(mem::uninitialized()) };

        let import_backing = ImportBacking::new(&module, &mut imports, &mut *vmctx)?;
        let backing = LocalBacking::new(&module, &import_backing, &mut *vmctx);

        // When Pin is stablized, this will use `Box::pinned` instead of `Box::new`.
        let mut inner = Box::new(InstanceInner {
            backing,
            import_backing,
            vmctx,
        });

        // Initialize the vm::Ctx in-place after the backing
        // has been boxed.
        *inner.vmctx =
            unsafe { vm::Ctx::new(&mut inner.backing, &mut inner.import_backing, &module) };

        let mut instance = Instance {
            module,
            inner,
            imports,
        };

        if let Some(start_index) = instance.module.start_func {
            instance.call_with_index(start_index, &[])?;
        }

        Ok(instance)
    }

    /// This returns the representation of a function that can be called
    /// safely.
    ///
    /// # Usage:
    /// ```
    /// # use wasmer_runtime_core::Instance;
    /// # use wasmer_runtime_core::error::CallResult;
    /// # fn call_foo(instance: &mut Instance) -> CallResult<()> {
    /// instance
    ///     .func("foo")?
    ///     .call(&[])?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn func(&mut self, name: &str) -> ResolveResult<Function> {
        let export_index =
            self.module
                .exports
                .get(name)
                .ok_or_else(|| ResolveError::ExportNotFound {
                    name: name.to_string(),
                })?;

        if let ExportIndex::Func(func_index) = export_index {
            let sig_index = *self
                .module
                .func_assoc
                .get(*func_index)
                .expect("broken invariant, incorrect func index");
            let signature = self.module.sig_registry.lookup_func_sig(sig_index);

            Ok(Function {
                signature,
                module: &self.module,
                instance_inner: &mut self.inner,
                func_index: *func_index,
            })
        } else {
            Err(ResolveError::ExportWrongType {
                name: name.to_string(),
            }
            .into())
        }
    }

    /// Call an exported webassembly function given the export name.
    /// Pass arguments by wrapping each one in the [`Value`] enum.
    /// The returned values are also each wrapped in a [`Value`].
    ///
    /// [`Value`]: enum.Value.html
    ///
    /// # Note:
    /// This returns `CallResult<Vec<Value>>` in order to support
    /// the future multi-value returns webassembly feature.
    ///
    /// # Usage:
    /// ```
    /// # use wasmer_runtime_core::types::Value;
    /// # use wasmer_runtime_core::error::Result;
    /// # use wasmer_runtime_core::Instance;
    /// # fn call_foo(instance: &mut Instance) -> Result<()> {
    /// // ...
    /// let results = instance.call("foo", &[Value::I32(42)])?;
    /// // ...
    /// # Ok(())
    /// # }
    /// ```
    pub fn call(&mut self, name: &str, args: &[Value]) -> CallResult<Vec<Value>> {
        let export_index =
            self.module
                .exports
                .get(name)
                .ok_or_else(|| ResolveError::ExportNotFound {
                    name: name.to_string(),
                })?;

        let func_index = if let ExportIndex::Func(func_index) = export_index {
            *func_index
        } else {
            return Err(CallError::Resolve(ResolveError::ExportWrongType {
                name: name.to_string(),
            })
            .into());
        };

        self.call_with_index(func_index, args)
    }

    /// Returns a immutable reference to the
    /// [`Ctx`] used by this Instance.
    ///
    /// [`Ctx`]: struct.Ctx.html
    pub fn context(&self) -> &vm::Ctx {
        &self.inner.vmctx
    }

    /// Returns a mutable reference to the
    /// [`Ctx`] used by this Instance.
    ///
    /// [`Ctx`]: struct.Ctx.html
    pub fn context_mut(&mut self) -> &mut vm::Ctx {
        &mut self.inner.vmctx
    }

    /// Returns a iterator over all of the items
    /// exported from this instance.
    pub fn exports(&mut self) -> ExportIter {
        ExportIter::new(&self.module, &mut self.inner)
    }

    /// The module used to instantiate this Instance.
    pub fn module(&self) -> Module {
        Module::new(Rc::clone(&self.module))
    }
}

impl Instance {
    fn call_with_index(&mut self, func_index: FuncIndex, args: &[Value]) -> CallResult<Vec<Value>> {
        let sig_index = *self
            .module
            .func_assoc
            .get(func_index)
            .expect("broken invariant, incorrect func index");
        let signature = self.module.sig_registry.lookup_func_sig(sig_index);

        if !signature.check_sig(args) {
            Err(ResolveError::Signature {
                expected: signature.clone(),
                found: args.iter().map(|val| val.ty()).collect(),
            })?
        }

        let vmctx = match func_index.local_or_import(&self.module) {
            LocalOrImport::Local(_) => &mut *self.inner.vmctx,
            LocalOrImport::Import(imported_func_index) => {
                self.inner.import_backing.functions[imported_func_index].vmctx
            }
        };

        let token = Token::generate();

        let returns = self.module.protected_caller.call(
            &self.module,
            func_index,
            args,
            &self.inner.import_backing,
            vmctx,
            token,
        )?;

        Ok(returns)
    }
}

impl InstanceInner {
    pub(crate) fn get_export_from_index(
        &mut self,
        module: &ModuleInner,
        export_index: &ExportIndex,
    ) -> Export {
        match export_index {
            ExportIndex::Func(func_index) => {
                let (func, ctx, signature) = self.get_func_from_index(module, *func_index);

                Export::Function {
                    func,
                    ctx: match ctx {
                        Context::Internal => Context::External(&mut *self.vmctx),
                        ctx @ Context::External(_) => ctx,
                    },
                    signature,
                }
            }
            ExportIndex::Memory(memory_index) => {
                let (local, ctx, memory) = self.get_memory_from_index(module, *memory_index);
                Export::Memory {
                    local,
                    ctx: match ctx {
                        Context::Internal => Context::External(&mut *self.vmctx),
                        ctx @ Context::External(_) => ctx,
                    },
                    memory,
                }
            }
            ExportIndex::Global(global_index) => {
                let (local, global) = self.get_global_from_index(module, *global_index);
                Export::Global { local, global }
            }
            ExportIndex::Table(table_index) => {
                let (local, ctx, table) = self.get_table_from_index(module, *table_index);
                Export::Table {
                    local,
                    ctx: match ctx {
                        Context::Internal => Context::External(&mut *self.vmctx),
                        ctx @ Context::External(_) => ctx,
                    },
                    table,
                }
            }
        }
    }

    fn get_func_from_index(
        &mut self,
        module: &ModuleInner,
        func_index: FuncIndex,
    ) -> (FuncPointer, Context, FuncSig) {
        let sig_index = *module
            .func_assoc
            .get(func_index)
            .expect("broken invariant, incorrect func index");

        let (func_ptr, ctx) = match func_index.local_or_import(module) {
            LocalOrImport::Local(local_func_index) => (
                module
                    .func_resolver
                    .get(&module, local_func_index)
                    .expect("broken invariant, func resolver not synced with module.exports")
                    .cast()
                    .as_ptr() as *const _,
                Context::Internal,
            ),
            LocalOrImport::Import(imported_func_index) => {
                let imported_func = &self.import_backing.functions[imported_func_index];
                (
                    imported_func.func as *const _,
                    Context::External(imported_func.vmctx),
                )
            }
        };

        let signature = module.sig_registry.lookup_func_sig(sig_index).clone();

        (unsafe { FuncPointer::new(func_ptr) }, ctx, signature)
    }

    fn get_memory_from_index(
        &mut self,
        module: &ModuleInner,
        mem_index: MemoryIndex,
    ) -> (MemoryPointer, Context, Memory) {
        match mem_index.local_or_import(module) {
            LocalOrImport::Local(local_mem_index) => {
                let vm_mem = &mut self.backing.vm_memories[local_mem_index];
                (
                    unsafe { MemoryPointer::new(vm_mem) },
                    Context::Internal,
                    *module
                        .memories
                        .get(local_mem_index)
                        .expect("broken invariant, memories"),
                )
            }
            LocalOrImport::Import(imported_mem_index) => {
                let &(_, mem) = &module
                    .imported_memories
                    .get(imported_mem_index)
                    .expect("missing imported memory index");
                let vm::ImportedMemory { memory, vmctx } =
                    &self.import_backing.memories[imported_mem_index];
                (
                    unsafe { MemoryPointer::new(*memory) },
                    Context::External(*vmctx),
                    *mem,
                )
            }
        }
    }

    fn get_global_from_index(
        &mut self,
        module: &ModuleInner,
        global_index: GlobalIndex,
    ) -> (GlobalPointer, GlobalDesc) {
        match global_index.local_or_import(module) {
            LocalOrImport::Local(local_global_index) => {
                let vm_global = &mut self.backing.vm_globals[local_global_index];
                (
                    unsafe { GlobalPointer::new(vm_global) },
                    module
                        .globals
                        .get(local_global_index)
                        .expect("broken invariant, globals")
                        .desc,
                )
            }
            LocalOrImport::Import(imported_global_index) => {
                let &(_, imported_global_desc) = &module
                    .imported_globals
                    .get(imported_global_index)
                    .expect("missing imported global index");
                let vm::ImportedGlobal { global } =
                    &self.import_backing.globals[imported_global_index];
                (
                    unsafe { GlobalPointer::new(*global) },
                    *imported_global_desc,
                )
            }
        }
    }

    fn get_table_from_index(
        &mut self,
        module: &ModuleInner,
        table_index: TableIndex,
    ) -> (TablePointer, Context, Table) {
        match table_index.local_or_import(module) {
            LocalOrImport::Local(local_table_index) => {
                let vm_table = &mut self.backing.vm_tables[local_table_index];
                (
                    unsafe { TablePointer::new(vm_table) },
                    Context::Internal,
                    *module
                        .tables
                        .get(local_table_index)
                        .expect("broken invariant, tables"),
                )
            }
            LocalOrImport::Import(imported_table_index) => {
                let &(_, tab) = &module
                    .imported_tables
                    .get(imported_table_index)
                    .expect("missing imported table index");
                let vm::ImportedTable { table, vmctx } =
                    &self.import_backing.tables[imported_table_index];
                (
                    unsafe { TablePointer::new(*table) },
                    Context::External(*vmctx),
                    *tab,
                )
            }
        }
    }
}

impl LikeNamespace for Instance {
    fn get_export(&mut self, name: &str) -> Option<Export> {
        let export_index = self.module.exports.get(name)?;

        Some(self.inner.get_export_from_index(&self.module, export_index))
    }
}

/// A representation of an exported WebAssembly function.
pub struct Function<'a> {
    signature: &'a FuncSig,
    module: &'a ModuleInner,
    instance_inner: &'a mut InstanceInner,
    func_index: FuncIndex,
}

impl<'a> Function<'a> {
    /// Call an exported webassembly function safely.
    ///
    /// Pass arguments by wrapping each one in the [`Value`] enum.
    /// The returned values are also each wrapped in a [`Value`].
    ///
    /// [`Value`]: enum.Value.html
    ///
    /// # Note:
    /// This returns `CallResult<Vec<Value>>` in order to support
    /// the future multi-value returns webassembly feature.
    ///
    /// # Usage:
    /// ```
    /// # use wasmer_runtime_core::Instance;
    /// # use wasmer_runtime_core::error::CallResult;
    /// # fn call_foo(instance: &mut Instance) -> CallResult<()> {
    /// instance
    ///     .func("foo")?
    ///     .call(&[])?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn call(&mut self, params: &[Value]) -> CallResult<Vec<Value>> {
        if !self.signature.check_sig(params) {
            Err(ResolveError::Signature {
                expected: self.signature.clone(),
                found: params.iter().map(|val| val.ty()).collect(),
            })?
        }

        let vmctx = match self.func_index.local_or_import(self.module) {
            LocalOrImport::Local(_) => &mut *self.instance_inner.vmctx,
            LocalOrImport::Import(imported_func_index) => {
                self.instance_inner.import_backing.functions[imported_func_index].vmctx
            }
        };

        let token = Token::generate();

        let returns = self.module.protected_caller.call(
            &self.module,
            self.func_index,
            params,
            &self.instance_inner.import_backing,
            vmctx,
            token,
        )?;

        Ok(returns)
    }

    pub fn signature(&self) -> &FuncSig {
        self.signature
    }

    pub fn raw(&self) -> *const vm::Func {
        match self.func_index.local_or_import(self.module) {
            LocalOrImport::Local(local_func_index) => self
                .module
                .func_resolver
                .get(self.module, local_func_index)
                .unwrap()
                .as_ptr(),
            LocalOrImport::Import(import_func_index) => {
                self.instance_inner.import_backing.functions[import_func_index].func
            }
        }
    }
}

#[doc(hidden)]
impl Instance {
    pub fn memory_offset_addr(&self, _: u32, _: usize) -> *const u8 {
        unimplemented!()
    }
}
