_vibe() {
    local current="${COMP_WORDS[COMP_CWORD]}"
    local options="--help --version --prompt --max-turns --max-price --max-tokens --enabled-tools --disabled-tools --output --agent --experimental-harness --legacy-harness --smart-approve --auto-approve --yolo --setup --check-upgrade --workdir --worktree --add-dir --trust --continue --resume"
    COMPREPLY=( $(compgen -W "${options}" -- "${current}") )
}
complete -F _vibe vibe
