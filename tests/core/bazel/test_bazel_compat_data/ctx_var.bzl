def _ctx_var_reader_impl(ctx):
    if ctx.var.get("BINDIR") == None:
        fail("ctx.var is missing BINDIR")
    if ctx.var.get("GENDIR") == None:
        fail("ctx.var is missing GENDIR")
    return []

ctx_var_reader = rule(
    implementation = _ctx_var_reader_impl,
)

def _ctx_var_mutator_impl(ctx):
    ctx.var["BINDIR"] = "mutated"
    return []

ctx_var_mutator = rule(
    implementation = _ctx_var_mutator_impl,
)
