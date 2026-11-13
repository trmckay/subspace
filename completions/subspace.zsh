function _subspace_project_names() {
    local -a names=()
    names=("${(@f)$(subspace project list 2>/dev/null)}")
    compadd -X 'project' -- "${names[@]}"
}

function _subspace_names() {
    local current_project
    current_project="${opt_args[-P]:-${opt_args[--project]}}"
    if [[ -z "$current_project" ]]; then
        local ws_current
        ws_current=$(subspace current 2>/dev/null)
        current_project="${ws_current%%/*}"
    fi
    local -a names=('@parent' '@root' '@current')
    if [[ -n "$current_project" ]]; then
        names+=("${(@f)$(subspace -P "$current_project" list 2>/dev/null)}")
    fi
    compadd -X 'workspace' -- "${names[@]}"
}

function _subspace_plain_names() {
    local current_project
    current_project="${opt_args[-P]:-${opt_args[--project]}}"
    if [[ -z "$current_project" ]]; then
        local ws_current
        ws_current=$(subspace current 2>/dev/null)
        current_project="${ws_current%%/*}"
    fi
    local -a names=()
    if [[ -n "$current_project" ]]; then
        names=("${(@f)$(subspace -P "$current_project" list 2>/dev/null)}")
    fi
    compadd -X 'workspace' -- "${names[@]}"
}

function _subspace_refs() {
    local -a names=('@parent' '@root' '@current')
    names+=("${(@f)$(git for-each-ref --format='%(refname:short)' refs/heads refs/remotes 2>/dev/null)}")
    compadd -X 'ref or @sigil' -- "${names[@]}"
}

function _subspace_range() {
    local current_project
    current_project="${opt_args[-P]:-${opt_args[--project]}}"
    if [[ -z "$current_project" ]]; then
        local ws_current
        ws_current=$(subspace current 2>/dev/null)
        current_project="${ws_current%%/*}"
    fi
    local -a names=('@parent' '@root' '@current')
    if [[ -n "$current_project" ]]; then
        names+=("${(@f)$(subspace -P "$current_project" list 2>/dev/null)}")
    fi
    if compset -P '*...'; then
        compadd -X 'right of ...' -- "${names[@]}"
    elif compset -P '*..'; then
        compadd -X 'right of ..' -- "${names[@]}"
    else
        compadd -X 'workspace, @sigil, or A..B / A...B' -r '.' -- "${names[@]}"
    fi
}

function _subspace_full_names() {
    local -a names=()
    local project
    for project in "${(@f)$(subspace project list 2>/dev/null)}"; do
        for ws in "${(@f)$(subspace -P "$project" list 2>/dev/null)}"; do
            names+=("$project/$ws")
        done
    done
    compadd -X 'project/workspace' -- "${names[@]}"
}

function _subspace_absolute_names() {
    _subspace_names
    _subspace_full_names
}

function _subspace_snapshot_names() {
    local current_project
    current_project="${opt_args[-P]:-${opt_args[--project]}}"
    if [[ -z "$current_project" ]]; then
        local ws_current
        ws_current=$(subspace current 2>/dev/null)
        current_project="${ws_current%%/*}"
    fi
    # A project can have no snapshots at all, so the split drops an empty
    # element. Then take the file-name column off each line.
    local -a names=()
    if [[ -n "$current_project" ]]; then
        names=(${(f)"$(subspace -P "$current_project" snapshot list 2>/dev/null)"})
        names=(${names[@]%% *})
    fi
    (( $#names )) || return 1
    compadd -X 'snapshot' -- "${names[@]}"
}

function _subspace() {
    local context state state_descr line
    typeset -A opt_args

    _arguments -C \
        '(-P --project -p)'{-P,--project}'[project name]:project:_subspace_project_names' \
        '(-p -P --project)-p[use CWD'"'"'s project in project scope]' \
        '(-w --workspace)'{-w,--workspace}'[workspace name or sigil]:workspace:_subspace_names' \
        '--color[when to use color output]:when:(always never auto)' \
        '--json[reserve stdout for one machine JSON document]' \
        '--preview[report the resolved mutation plan without changing state]' \
        '(--no-interactive)--interactive[prompt for confirmation]' \
        '(--interactive)--no-interactive[never prompt for confirmation]' \
        '(-v --verbose)'{-q,--quiet}'[suppress informational output]' \
        '(-q --quiet)'{-v,--verbose}'[emit additional diagnostic output]' \
        '1:command:->subcmd' \
        '*::args:->args'

    case $state in
        subcmd)
            local -a subcmds=(
                'list:list workspace names'
                'show:print workspace summaries and changed paths'
                'inspect:inspect a workspace or project'
                'compare:compare workspaces'
                'capabilities:report the machine protocol'
                'add:add a workspace from a ref or another workspace'
                'branch:add a workspace nested under the current one'
                'clone:copy current workspace tip and parent fork point'
                'current:print the workspace containing CWD'
                'default:print or set the project default workspace'
                'branch-prefix:print or set the project branch prefix'
                'workspace-branch:print or set the branch recorded for workspace repair'
                'project:project commands (remove, list, clone, init, current)'
                'diff:run git diff with workspace/sigil expansion'
                'log:run git log oldest first with workspace/sigil expansion'
                'merge:merge workspace into current'
                'path:print workspace or project path'
                'children:show children of current workspace'
                'siblings:show sibling workspaces'
                'parent:show parent workspace'
                'root:show root workspace of current tree'
                'tree:show workspace tree'
                'rebase:rebase workspace branch onto its base'
                'integration:continue or abort a stopped merge or rebase'
                'remove:remove a workspace worktree'
                'rename:rename a workspace'
                'repair:prune dead metadata and fix or rebuild broken worktree links'
                'snapshot:archive a commit, list snapshots, or restore one'
            )
            _describe -t commands 'command' subcmds ;;
        args)
            local cmd="${line[1]}"
            case $cmd in
                add)
                    _arguments \
                        '1:name:' \
                        '2:ref:_subspace_refs' \
                        '--parent[parent workspace]:parent:_subspace_names' \
                        '--parent-of[insert new workspace as parent of CHILD]:child:_subspace_names' \
                        '--branch[new branch name]:branch:' \
                        '--detach[check out a detached HEAD]' \
                        '--unstaged[carry unstaged changes and untracked files from parent workspace]' ;;
                branch | clone)
                    _arguments \
                        '1:name:' \
                        '2:ref:_subspace_refs' \
                        '--branch[new branch name]:branch:' \
                        '--detach[check out a detached HEAD]' \
                        '--unstaged[carry unstaged changes and untracked files from current workspace]' ;;
                project)
                    _arguments -C \
                        '1:action:(remove rename move list clone init current)' \
                        '*::args:->projectargs'
                    case $state in
                        projectargs)
                            local paction="${line[1]}"
                            case $paction in
                                remove)
                                    _arguments \
                                        '1:name:_subspace_project_names' \
                                        '--remove-directory[also delete the project directory]' ;;
                                rename)
                                    _arguments \
                                        '1:name:_subspace_project_names' \
                                        '2:new name:' \
                                        '--no-move[keep the project directory where it is]' ;;
                                move)
                                    _arguments \
                                        '1:name:_subspace_project_names' \
                                        '2:dir:_files -/' ;;
                                list)
                                    : ;;
                                clone)
                                    _arguments \
                                        '1:remote:' \
                                        '2::dir:_files -/' \
                                        '--name[name for the project]:name:' ;;
                                init)
                                    _arguments \
                                        '1:project path:_files -/' \
                                        '--name[project name]:name:' ;;
                            esac ;;
                    esac ;;
                current | capabilities)
                    : ;;
                inspect)
                    _arguments '1:target:(workspace project)' ;;
                compare)
                    _arguments '1:workspace:_subspace_names' ;;
                integration)
                    _arguments '1:action:(continue abort)' ;;
                default)
                    _arguments \
                        '--set[set the default workspace]:workspace:_subspace_plain_names' ;;
                branch-prefix)
                    _arguments \
                        '--set[set the branch prefix]:prefix:' \
                        '--unset[clear the branch prefix]' ;;
                workspace-branch)
                    _arguments \
                        '--set[set the recorded branch]:branch:' \
                        '--unset[clear the recorded branch]' ;;
                snapshot)
                    _arguments -C \
                        '1:action:(create list restore)' \
                        '*::args:->snapshotargs'
                    case $state in
                        snapshotargs)
                            local saction="${line[1]}"
                            case $saction in
                                create)
                                    _arguments \
                                        '1::ref:_subspace_refs' ;;
                                list)
                                    : ;;
                                restore)
                                    _arguments \
                                        '1:snapshot:_subspace_snapshot_names' \
                                        '2::name:' ;;
                            esac ;;
                    esac ;;
                diff)
                    _arguments \
                        '--color[when to use color output]:when:(always never auto)' \
                        '--output[git output mode]:mode:(patch stat name-only)' \
                        '*--path[include a repository-relative path]:path:_files' \
                        '--parent[@parent...@current]' \
                        '--root[@root...@current]' \
                        '--in[@parent...<ws>]:workspace:_subspace_names' \
                        '--from[<ws>...@current]:workspace:_subspace_names' \
                        '--to[@current...<ws>]:workspace:_subspace_names' \
                        '1::ref or A..B / A...B range:_subspace_range' \
                        '*:git diff args:' ;;
                log)
                    _arguments \
                        '--color[when to use color output]:when:(always never auto)' \
                        '--output[git output mode]:mode:(patch stat name-only)' \
                        '*--path[include a repository-relative path]:path:_files' \
                        '--parent[@parent...@current (cherry-pick filtered)]' \
                        '--root[@root...@current (cherry-pick filtered)]' \
                        '--in[@parent...<ws> (cherry-pick filtered)]:workspace:_subspace_names' \
                        '--from[<ws>...@current (cherry-pick filtered)]:workspace:_subspace_names' \
                        '--to[@current...<ws> (cherry-pick filtered)]:workspace:_subspace_names' \
                        '1::ref or A..B / A...B range:_subspace_range' \
                        '*:git log args:' ;;
                merge)
                    _arguments \
                        '1:source workspace:_subspace_names' \
                        '(-m --squash)'{-m,--squash}'[squash selected commits with this commit message]:message:' \
                        '--all[replay all source commits since the common ancestor]' \
                        '--unstaged[also bring unstaged changes and untracked files]' \
                        '--remove[remove the source workspace after a successful merge]' \
                        '--delete-branch[delete the source branch with the source workspace]' ;;
                list)
                    _arguments \
                        '--parents[print "name<TAB>parent" per workspace]' ;;
                show)
                    : ;;
                children | siblings)
                    : ;;
                parent)
                    : ;;
                root)
                    : ;;
                tree)
                    : ;;
                path)
                    : ;;
                rebase)
                    _arguments \
                        '1::base branch:_subspace_names' \
                        '-i[rebase interactively (git rebase -i)]' \
                        '--all[replay every commit ahead of the base, not just those ahead of the branch point]' ;;
                remove)
                    _arguments \
                        '1:target:_subspace_absolute_names' \
                        '--keep-branch[keep the git branch after the worktree goes]' \
                        '--force[remove a dirty or locked worktree]' \
                        '(-r --recursive)'{-r,--recursive}'[remove workspace and all descendants]' ;;
                rename)
                    _arguments \
                        '1:target:_subspace_absolute_names' \
                        '2:new name:' ;;
                repair)
                    _arguments \
                        '1::project:_subspace_project_names' \
                        '--dry-run[report what changes, and change nothing]' ;;

            esac ;;
    esac
}

compdef _subspace subspace ws
