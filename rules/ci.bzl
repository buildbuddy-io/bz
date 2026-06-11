# Local CI label helpers used by tests after removing Target Determinator.

def _flatten(labels):
    out = []
    for label in labels:
        if label == None:
            continue
        if type(label) == type([]):
            out.extend(_flatten(label))
        else:
            out.append(label)
    return out

def _label(name, *labels):
    nested = _flatten(labels)
    if not nested:
        return name
    return [name] + nested

def _labels(*labels):
    return _flatten(labels)

def _remove_labels(*_labels):
    return []

def _skip_target():
    return "ci:skip"

def _skip_test():
    return "ci:skip_test"

def _overwrite():
    return "ci:overwrite"

def _mode(mode):
    return "ci:mode:{}".format(mode)

def _aarch64(*labels):
    return _label("ci:aarch64", *labels)

def _linux(*labels):
    return _label("ci:linux", *labels)

def _mac(*labels):
    return _label("ci:mac", *labels)

def _opt(*labels):
    return _label("ci:opt", *labels)

def _windows(*labels):
    return _label("ci:windows", *labels)

ci = struct(
    aarch64 = _aarch64,
    labels = _labels,
    linux = _linux,
    mac = _mac,
    mode = _mode,
    opt = _opt,
    overwrite = _overwrite,
    skip_test = _skip_test,
    remove_labels = _remove_labels,
    skip_target = _skip_target,
    windows = _windows,
)

def ci_hint(**_kwargs):
    pass
