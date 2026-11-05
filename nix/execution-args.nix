# Admission is an explicit start policy, independent of native containment.
# Callers append `--` and the payload argv, escaping each argument for a shell.
{
  admission ? false,
  contractName ? "tool",
  contract,
  runtimeMaxSec ? 1800,
}:
assert builtins.isInt runtimeMaxSec && runtimeMaxSec >= 1 && runtimeMaxSec <= 86400;
assert !admission || contractName != "";
  (
    if admission
    then ["admission" "exec" "--contract" contractName]
    else [
      "exec"
      "--slice"
      contract.slice
      "--memory-max"
      (toString contract.memory_max)
      "--memory-swap-max"
      (toString contract.memory_swap_max)
    ]
  )
  ++ ["--runtime-max-sec" (toString runtimeMaxSec)]
