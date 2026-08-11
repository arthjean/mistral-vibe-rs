Register-ArgumentCompleter -Native -CommandName vibe -ScriptBlock {
    param($wordToComplete)
    @(
        "--version", "--prompt", "--output", "--resume", "--continue", "--workdir",
        "--add-dir", "--trust", "--agent", "--enabled-tools", "--disabled-tools",
        "--max-turns", "--max-tokens", "--max-price", "--auto-approve", "--setup",
        "--check-upgrade", "--worktree", "--help"
    ) | Where-Object { $_ -like "$wordToComplete*" } | ForEach-Object {
        [System.Management.Automation.CompletionResult]::new($_, $_, "ParameterName", $_)
    }
}
