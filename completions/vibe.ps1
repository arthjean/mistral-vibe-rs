Register-ArgumentCompleter -Native -CommandName vibe -ScriptBlock {
    param($wordToComplete)
    @(
        "--help", "--version", "--prompt", "--max-turns", "--max-price",
        "--max-tokens", "--enabled-tools", "--disabled-tools", "--output",
        "--agent", "--experimental-harness", "--legacy-harness",
        "--smart-approve", "--auto-approve", "--yolo", "--setup", "--check-upgrade",
        "--workdir", "--worktree", "--add-dir", "--trust", "--continue",
        "--resume"
    ) | Where-Object { $_ -like "$wordToComplete*" } | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, "ParameterName", $_)
    }
}
