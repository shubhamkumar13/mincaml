use cranelift_codegen::binemit::NullTrapSink;
use cranelift_codegen::entity::EntityRef;
use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::entities::{Block, FuncRef, SigRef, Value};
use cranelift_codegen::ir::types::*;
use cranelift_codegen::ir::MemFlags;
use cranelift_codegen::ir::{AbiParam, InstBuilder, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings;
use cranelift_codegen::verifier::verify_function;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{default_libcall_names, DataId, FuncId, Linkage, Module};
use cranelift_object::{ObjectBackend, ObjectBuilder, ObjectProduct};

use fxhash::FxHashMap;

use crate::cg_types::RepType;
use crate::common::{BinOp, Cmp, FloatBinOp, IntBinOp};
use crate::ctx::{Ctx, VarId};
use crate::lower;
use crate::type_check;

pub fn codegen(ctx: &mut Ctx, funs: &[lower::Fun], main_id: VarId, dump: bool) -> Vec<u8> {
    // Module and FunctionBuilderContext are used for the whole compilation unit. Each function
    // gets its own FunctionBuilder.
    let codegen_flags: settings::Flags = settings::Flags::new(settings::builder());
    let mut module: Module<ObjectBackend> = Module::new(ObjectBuilder::new(
        // How does this know I'm building for x86_64 Linux?
        cranelift_native::builder().unwrap().finish(codegen_flags),
        [1, 2, 3, 4, 5, 6, 7, 8], // TODO: what is this?
        default_libcall_names(),
    ));

    let mut fn_builder_ctx: FunctionBuilderContext = FunctionBuilderContext::new();

    // Declare malloc at module-level and pass the id to code gen to be able to generate malloc
    // calls.
    let malloc_id = declare_malloc(&mut module);

    // Global env is not mutable as we never add anything to it. Declarations in basic blocks are
    // done directly using the FunctionBuilder. When a variable isn't bound in 'env' it assumes
    // that the variable has already been declared directly using the FunctionBuilder.
    //
    // For function arguments we clone it in every function, add the arguments, and then keep using
    // it in an immutable way.
    let (env, main_fun_id) = init_module_env(ctx, &mut module, funs, main_id);

    // Generate code for functions
    for fun in funs {
        codegen_fun(
            ctx,
            &mut module,
            &env,
            malloc_id,
            fun,
            &mut fn_builder_ctx,
            dump,
        );
    }

    // Generate main
    make_main(&mut module, &mut fn_builder_ctx, main_fun_id, dump);

    module.finalize_definitions();

    let object: ObjectProduct = module.finish();
    object.emit().unwrap()
}

// We only support such platforms.
const WORD_SIZE: u8 = 8;

// Used to map function arguments and globals (other functions and closures in the module,
// built-ins) to their values.
#[derive(Clone)]
struct Env(FxHashMap<VarId, VarVal>);

#[derive(Debug, Clone)]
enum VarVal {
    // A function argument.
    Arg(Value),
    // Variable is a reference to a function. Get a reference to it using `declare_data_in_func`
    // and a value of it using `global_value`.
    Fun(FuncId),
    // Variable is a reference to a data object (i.e. a closure). Get a reference to it using
    // `declare_data_in_func` and a value of it using `global_value`.
    Data(DataId),
}

impl Env {
    fn new() -> Self {
        Env(Default::default())
    }

    fn add_arg(&mut self, var: VarId, val: Value) {
        self.0.insert(var, VarVal::Arg(val));
    }

    fn add_fun(&mut self, var: VarId, val: FuncId) {
        self.0.insert(var, VarVal::Fun(val));
    }

    fn add_data(&mut self, var: VarId, val: DataId) {
        self.0.insert(var, VarVal::Data(val));
    }

    fn get_fun(&self, var: VarId) -> Option<FuncId> {
        match self.0.get(&var) {
            Some(VarVal::Fun(fun_id)) => Some(*fun_id),
            _ => None,
        }
    }

    fn use_var(
        &mut self, ctx: &Ctx, module: &Module<ObjectBackend>, builder: &mut FunctionBuilder,
        var: VarId,
    ) -> Value {
        let val = self.0.get(&var).cloned();

        // NB. caching fun and data refs below won't work, as the values may be defined in a block
        // and used in another block which is not dominated by the defining block. Example:
        //
        // block 0 (entry) -> block 1 (defines v11)
        //                 -> block 2 -> block 4
        //                            -> block 3 (uses v11)
        //
        // Here we can't use v11 which is defined in v11, we have to re-define it in v11.
        //
        // Below we simply redefine it in all use sites.

        match val {
            Some(VarVal::Arg(arg)) => arg,
            Some(VarVal::Fun(fun_id)) => {
                let fun_ref = module.declare_func_in_func(fun_id, builder.func);
                // self.0.insert(var, VarVal::KnownFun(fun_ref));
                builder.ins().func_addr(I64, fun_ref)
            }
            Some(VarVal::Data(data_id)) => {
                let data_ref = module.declare_data_in_func(data_id, builder.func);
                let val = builder.ins().global_value(I64, data_ref);
                // self.0.insert(var, VarVal::Known(val));
                val
            }
            None => {
                // Should be a variable declared and defined before.
                let var = Variable::new(ctx.get_var(var).get_uniq().0.get() as usize);
                builder.use_var(var)
            }
        }
    }
}

fn declare_malloc(module: &mut Module<ObjectBackend>) -> FuncId {
    module
        .declare_function(
            "malloc",
            Linkage::Import,
            &Signature {
                params: vec![AbiParam::new(I64)],
                returns: vec![AbiParam::new(I64)],
                call_conv: CallConv::SystemV,
            },
        )
        .unwrap()
}

fn init_module_env(
    ctx: &mut Ctx, module: &mut Module<ObjectBackend>, funs: &[lower::Fun], main_id: VarId,
) -> (Env, FuncId) {
    let mut main_fun_id: Option<FuncId> = None;
    let mut env = Env::new();

    // Declare built-ins
    for (builtin_var_id, _ty_id) in ctx.builtins() {
        let var = ctx.get_var(*builtin_var_id);
        let name = var.symbol_name();

        let id: DataId = module
            .declare_data(&*name, Linkage::Import, false, false, None)
            .unwrap();
        env.add_data(*builtin_var_id, id);
    }

    // Declare functions
    for lower::Fun {
        name,
        args,
        return_type,
        ..
    } in funs
    {
        let params: Vec<AbiParam> = args
            .iter()
            .map(|arg| AbiParam::new(rep_type_abi(ctx.var_rep_type(*arg))))
            .collect();

        let returns: Vec<AbiParam> = vec![AbiParam::new(rep_type_abi(*return_type))];

        let sig = Signature {
            params,
            returns,
            call_conv: CallConv::SystemV,
        };

        let id: FuncId = module
            .declare_function(&*ctx.get_var(*name).name(), Linkage::Local, &sig)
            .unwrap();

        if *name == main_id {
            main_fun_id = Some(id);
            assert!(args.is_empty());
            assert_eq!(*return_type, RepType::Word);
        }

        env.add_fun(*name, id);
    }

    let main_fun_id = main_fun_id.expect("Can't find main function");

    (env, main_fun_id)
}

fn codegen_fun(
    ctx: &mut Ctx, module: &mut Module<ObjectBackend>, global_env: &Env, malloc_id: FuncId,
    fun: &lower::Fun, fn_builder_ctx: &mut FunctionBuilderContext, dump: bool,
) {
    let lower::Fun {
        name,
        args,
        blocks,
        return_type,
    } = fun;

    let mut context = module.make_context();

    // TODO: We already created a signature for this function, in the forward declaration in
    // `init_module_env`. Is there a way to reuse it here?
    let signature: &mut Signature = &mut context.func.signature;
    for arg in args {
        let arg_type = ctx.var_rep_type(*arg);
        let arg_abi_type = rep_type_abi(arg_type);
        signature.params.push(AbiParam::new(arg_abi_type));
    }
    signature
        .returns
        .push(AbiParam::new(rep_type_abi(*return_type)));

    // The function is forward-declared in `init_module_env`, use it.
    let func_id = global_env
        .get_fun(*name)
        .expect("Can't find FuncId of function");

    // TODO: Only do this for functions that allocate
    let malloc: FuncRef = module.declare_func_in_func(malloc_id, &mut context.func);

    let mut builder: FunctionBuilder = FunctionBuilder::new(&mut context.func, fn_builder_ctx);

    let mut label_to_block: FxHashMap<lower::Label, Block> = Default::default();

    for block in blocks {
        let cl_block = builder.create_block();
        label_to_block.insert(block.label, cl_block);
    }

    let entry_block = *label_to_block.get(&blocks[0].label).unwrap();
    builder.switch_to_block(entry_block);
    builder.append_block_params_for_function_params(entry_block);

    // Add arguments to env
    let mut env = global_env.clone();
    for (arg_idx, arg) in args.iter().enumerate() {
        let val = builder.block_params(entry_block)[arg_idx];
        env.add_arg(*arg, val);
    }

    for lower::Block { label, stmts, exit } in blocks {
        let mut cl_block = *label_to_block.get(label).unwrap();
        builder.switch_to_block(cl_block);

        for stmt in stmts {
            // let mut s = String::new();
            // asgn.pp(&ctx, &mut s);
            // println!("stmt: {}", s);

            match stmt {
                lower::Stmt::Asgn(lower::Asgn { lhs, rhs }) => {
                    let (block, val) =
                        codegen_expr(ctx, &module, cl_block, &mut builder, &mut env, malloc, rhs);
                    cl_block = block;

                    let lhs_cl_var = Variable::new(ctx.get_var(*lhs).get_uniq().0.get() as usize);
                    let lhs_abi_type = rep_type_abi(ctx.var_rep_type(*lhs));
                    builder.declare_var(lhs_cl_var, lhs_abi_type);
                    builder.def_var(lhs_cl_var, val.unwrap());
                }
                lower::Stmt::Expr(expr) => {
                    let (block, _) =
                        codegen_expr(ctx, &module, cl_block, &mut builder, &mut env, malloc, expr);
                    cl_block = block;
                }
            }
        }

        match exit {
            lower::Exit::Return(var) => {
                let var = env.use_var(ctx, &module, &mut builder, *var);
                builder.ins().return_(&[var]);
            }
            lower::Exit::Branch {
                v1,
                v2,
                cond,
                then_label,
                else_label,
            } => {
                let comp_type = RepType::from(&*ctx.var_type(*v1));
                let v1 = env.use_var(ctx, &module, &mut builder, *v1);
                let v2 = env.use_var(ctx, &module, &mut builder, *v2);

                let then_block = *label_to_block.get(then_label).unwrap();
                let else_block = *label_to_block.get(else_label).unwrap();

                match comp_type {
                    RepType::Word => {
                        let cond = word_cond(*cond);
                        builder.ins().br_icmp(cond, v1, v2, then_block, &[]);
                    }
                    RepType::Float => {
                        let cond = float_cond(*cond);
                        let cmp = builder.ins().fcmp(cond, v1, v2);
                        builder.ins().brnz(cmp, then_block, &[]);
                        // NB: For some reason the code below doesn't work. Would be good to know
                        // why.
                        // let flags = builder.ins().ffcmp(v1, v2);
                        // builder.ins().brff(cond, flags, then_block, &[]);
                    }
                }

                builder.ins().jump(else_block, &[]);
            }
            lower::Exit::Jump(label) => {
                let cl_block = *label_to_block.get(label).unwrap();
                // Not sure about the arguments here...
                builder.ins().jump(cl_block, &[]);
            }
        }

        builder.seal_block(cl_block);
    }

    // println!("Function before finalizing:");
    // println!("{}", builder.display(None));
    builder.finalize();

    let flags = settings::Flags::new(settings::builder());
    let res = verify_function(&context.func, &flags);

    if dump {
        println!("{}", context.func.display(None));
    }
    if let Err(errors) = res {
        println!("{}", errors);
    }

    module
        .define_function(func_id, &mut context, &mut NullTrapSink {})
        .unwrap();
    module.clear_context(&mut context);
}

fn codegen_expr(
    ctx: &mut Ctx, module: &Module<ObjectBackend>, block: Block, builder: &mut FunctionBuilder,
    env: &mut Env, malloc: FuncRef, rhs: &lower::Expr,
) -> (Block, Option<Value>) {
    match rhs {
        lower::Expr::Atom(lower::Atom::Unit) => (block, Some(builder.ins().iconst(I64, 0))),
        lower::Expr::Atom(lower::Atom::Int(i)) => (block, Some(builder.ins().iconst(I64, *i))),
        lower::Expr::Atom(lower::Atom::Float(f)) => (block, Some(builder.ins().f64const(*f))),
        lower::Expr::Atom(lower::Atom::Var(var)) => {
            (block, Some(env.use_var(ctx, module, builder, *var)))
        }

        lower::Expr::IBinOp(BinOp { op, arg1, arg2 }) => {
            let arg1 = env.use_var(ctx, module, builder, *arg1);
            let arg2 = env.use_var(ctx, module, builder, *arg2);
            let val = match op {
                IntBinOp::Add => builder.ins().iadd(arg1, arg2),
                IntBinOp::Sub => builder.ins().isub(arg1, arg2),
                IntBinOp::Mul => builder.ins().imul(arg1, arg2),
                IntBinOp::Div => builder.ins().sdiv(arg1, arg2),
            };
            (block, Some(val))
        }

        lower::Expr::FBinOp(BinOp { op, arg1, arg2 }) => {
            let arg1 = env.use_var(ctx, module, builder, *arg1);
            let arg2 = env.use_var(ctx, module, builder, *arg2);
            let val = match op {
                FloatBinOp::Add => builder.ins().fadd(arg1, arg2),
                FloatBinOp::Sub => builder.ins().fsub(arg1, arg2),
                FloatBinOp::Mul => builder.ins().fmul(arg1, arg2),
                FloatBinOp::Div => builder.ins().fdiv(arg1, arg2),
            };
            (block, Some(val))
        }

        lower::Expr::Neg(var) => {
            let arg = env.use_var(ctx, module, builder, *var);
            (block, Some(builder.ins().ineg(arg)))
        }

        lower::Expr::FNeg(var) => {
            let arg = env.use_var(ctx, module, builder, *var);
            (block, Some(builder.ins().fneg(arg)))
        }

        lower::Expr::App(fun, args, ret_type) => {
            let params: Vec<AbiParam> = args
                .iter()
                .map(|arg| {
                    let arg_ty = ctx.var_rep_type(*arg);
                    AbiParam::new(rep_type_abi(arg_ty))
                })
                .collect();

            let returns: Vec<AbiParam> = vec![AbiParam::new(rep_type_abi(*ret_type))];

            // TODO: Apparently cranelift doesn't intern these signatures so if we add `int -> int`
            // many times we get many `int -> int` signatures in the module. Would be good to cache
            // and reuse SigRefs.
            let fun_sig = Signature {
                params,
                returns,
                call_conv: CallConv::SystemV,
            };
            let fun_sig_ref: SigRef = builder.import_signature(fun_sig);

            let callee = env.use_var(ctx, module, builder, *fun);

            let arg_vals: Vec<Value> = args
                .iter()
                .map(|arg| env.use_var(ctx, module, builder, *arg))
                .collect();
            let call = builder.ins().call_indirect(fun_sig_ref, callee, &arg_vals);
            (block, Some(builder.inst_results(call)[0]))
        }

        lower::Expr::Tuple { len } => {
            let malloc_arg = builder
                .ins()
                .iconst(I64, *len as i64 * i64::from(WORD_SIZE));
            let malloc_call = builder.ins().call(malloc, &[malloc_arg]);
            let tuple = builder.inst_results(malloc_call)[0];
            (block, Some(tuple))
        }

        lower::Expr::TuplePut(tuple, idx, val) => {
            let tuple = env.use_var(ctx, module, builder, *tuple);
            let arg = env.use_var(ctx, module, builder, *val);
            builder.ins().store(
                MemFlags::new(),
                arg,
                tuple,
                (idx * usize::from(WORD_SIZE)) as i32,
            );
            (block, None)
        }

        lower::Expr::TupleGet(tuple, idx) => {
            let tuple_type = ctx.var_type(*tuple);
            let elem_type = match &*tuple_type {
                type_check::Type::Tuple(args) => rep_type_abi(RepType::from(&args[*idx])),
                type_check::Type::Fun { .. } => {
                    // NOTE DISGUSTING HACK: This case happens after closure conversion where we
                    // turn functions into tuples (closures) and in application code when we see
                    //
                    //   f x
                    //
                    // we instead do
                    //
                    //   f.0 x
                    //
                    // Note sure how to best implement/fix this, so for now we allow this case.
                    I64
                }
                other => panic!("Non-tuple in tuple position: {:?}", other),
            };

            let tuple = env.use_var(ctx, module, builder, *tuple);

            let val = builder.ins().load(
                elem_type,
                MemFlags::new(),
                tuple,
                (idx * usize::from(WORD_SIZE)) as i32,
            );
            (block, Some(val))
        }

        lower::Expr::ArrayAlloc { len, elem } => {
            // Allocate array, move elements to array locations in a loop. Why lower it this much
            // here? Reasons:
            //
            // - I don't want to introduce mutable variables in lower or anormal.
            //
            // - I want to generate code as early as possible in compilation and will probably
            //   merge anormal and lower at some point and do more work here.
            //
            // - I want to learn more about cranelift, especially how to deal with mutable
            //   variables and how to introduce loops.
            //
            // NB. update varibles with `def_var`

            let len_val = env.use_var(ctx, module, builder, *len);
            let word_size = builder.ins().iconst(I64, i64::from(WORD_SIZE));
            let size_val = builder.ins().imul(len_val, word_size);
            let malloc_call = builder.ins().call(malloc, &[size_val]);
            let array = builder.inst_results(malloc_call)[0];

            let elem_val = env.use_var(ctx, module, builder, *elem);

            let array_bound_uniq = ctx.fresh_uniq();
            let array_bound_var = Variable::new(array_bound_uniq.0.get() as usize);
            builder.declare_var(array_bound_var, I64);
            let array_bound_val = builder.ins().iadd(array, size_val);
            builder.def_var(array_bound_var, array_bound_val);

            let loop_block = builder.create_block();
            let loop_doit_block = builder.create_block(); // block after loop condition (loop body)
            let cont_block = builder.create_block();

            // Introduce a variable for current index
            let idx_uniq = ctx.fresh_uniq();
            let idx_var = Variable::new(idx_uniq.0.get() as usize);
            builder.declare_var(idx_var, I64);
            builder.def_var(idx_var, array);
            builder.ins().jump(loop_block, &[]);
            builder.seal_block(block);

            builder.switch_to_block(loop_block);
            // if loc == array_bound { jmp cont; }
            let idx_val = builder.use_var(idx_var);
            builder
                .ins()
                .br_icmp(IntCC::Equal, idx_val, array_bound_val, cont_block, &[]);
            builder.ins().fallthrough(loop_doit_block, &[]);

            builder.switch_to_block(loop_doit_block);
            // If not, then move 'elem' to the location, bump index, loop
            builder.ins().store(MemFlags::new(), elem_val, idx_val, 0);
            let word_size = builder.ins().iconst(I64, i64::from(WORD_SIZE));
            let next_idx = builder.ins().iadd(idx_val, word_size);
            builder.def_var(idx_var, next_idx);
            builder.ins().jump(loop_block, &[]);

            builder.seal_block(loop_block);
            builder.seal_block(loop_doit_block);

            builder.switch_to_block(cont_block);

            (cont_block, Some(array))
        }

        lower::Expr::ArrayGet(array, idx) => {
            let var_type = ctx.var_type(*array);
            let elem_type = match &*var_type {
                type_check::Type::Array(elem_type) => rep_type_abi(RepType::from(&**elem_type)),
                _ => panic!("Non-array in array location"),
            };

            let array = env.use_var(ctx, module, builder, *array);
            let idx = env.use_var(ctx, module, builder, *idx);
            let word_size = builder.ins().iconst(I64, i64::from(WORD_SIZE));
            let offset = builder.ins().imul(idx, word_size);
            (
                block,
                Some(
                    builder
                        .ins()
                        .load_complex(elem_type, MemFlags::new(), &[array, offset], 0),
                ),
            )
        }

        lower::Expr::ArrayPut(array, idx, val) => {
            let array = env.use_var(ctx, module, builder, *array);
            let idx = env.use_var(ctx, module, builder, *idx);
            let val = env.use_var(ctx, module, builder, *val);
            let word_size = builder.ins().iconst(I64, 8);
            let offset = builder.ins().imul(idx, word_size);
            builder
                .ins()
                .store_complex(MemFlags::new(), val, &[array, offset], 0);
            let ret = builder.ins().iconst(I64, 0);
            (block, Some(ret))
        }
    }
}

fn make_main(
    module: &mut Module<ObjectBackend>, fun_ctx: &mut FunctionBuilderContext, main_id: FuncId,
    dump: bool,
) {
    let mut context = module.make_context();
    context.func.signature = Signature {
        params: vec![],
        returns: vec![AbiParam::new(I32)],
        call_conv: CallConv::SystemV,
    };
    let main_func_id: FuncId = module
        .declare_function("main", Linkage::Export, &context.func.signature)
        .unwrap();
    let mut builder: FunctionBuilder = FunctionBuilder::new(&mut context.func, fun_ctx);
    let block = builder.create_block();
    builder.switch_to_block(block);
    let expr_func_ref: FuncRef = module.declare_func_in_func(main_id, builder.func);
    builder.ins().call(expr_func_ref, &[]);
    let ret = builder.ins().iconst(I32, 0);
    builder.ins().return_(&[ret]);
    builder.seal_block(block);

    let flags = settings::Flags::new(settings::builder());
    let res = verify_function(&context.func, &flags);

    if dump {
        println!("{}", context.func.display(None));
    }
    if let Err(errors) = res {
        println!("{}", errors);
    }

    module
        .define_function(main_func_id, &mut context, &mut NullTrapSink {})
        .unwrap();
    module.clear_context(&mut context);
}

fn rep_type_abi(ty: RepType) -> Type {
    match ty {
        RepType::Word => I64,
        RepType::Float => F64,
    }
}

fn word_cond(cond: Cmp) -> IntCC {
    match cond {
        Cmp::Equal => IntCC::Equal,
        Cmp::NotEqual => IntCC::NotEqual,
        Cmp::LessThan => IntCC::SignedLessThan,
        Cmp::LessThanOrEqual => IntCC::SignedLessThanOrEqual,
        Cmp::GreaterThan => IntCC::SignedGreaterThan,
        Cmp::GreaterThanOrEqual => IntCC::SignedGreaterThanOrEqual,
    }
}

fn float_cond(cond: Cmp) -> FloatCC {
    match cond {
        Cmp::Equal => FloatCC::Equal,
        Cmp::NotEqual => FloatCC::NotEqual,
        Cmp::LessThan => FloatCC::LessThan,
        Cmp::LessThanOrEqual => FloatCC::LessThanOrEqual,
        Cmp::GreaterThan => FloatCC::GreaterThan,
        Cmp::GreaterThanOrEqual => FloatCC::GreaterThanOrEqual,
    }
}
