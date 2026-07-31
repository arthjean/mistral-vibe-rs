_vibe() {
    local current="${COMP_WORDS[COMP_CWORD]}"
    local options="--version --prompt --output --resume --continue --workdir --add-dir --trust --agent --enabled-tools --disabled-tools --max-turns --max-tokens --max-price --auto-approve --setup --check-upgrade --telemetry --worktree --help"
    COMPREPLY=( $(compgen -W "${options}" -- "${current}") )
}
complete -F _vibe vibe
