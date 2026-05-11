Write-Host "You can launch claude or codex here."

function _xmlui_mark_agent {
    param([string]$Provider)
    if (-not $env:XMLUI_DESKTOP_AGENT_HINT) { return }
    $parent = Split-Path -Parent $env:XMLUI_DESKTOP_AGENT_HINT
    if ($parent) {
        try { New-Item -ItemType Directory -Force -Path $parent | Out-Null } catch { return }
    }
    Set-Content -Path $env:XMLUI_DESKTOP_AGENT_HINT -Value ('{"provider":"' + $Provider + '"}')
}

function _xmlui_run_real {
    param([string]$Name, [object[]]$ForwardArgs)
    $real = Get-Command -Name $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $real) {
        Write-Error "$Name not found on PATH"
        return
    }
    & $real.Source @ForwardArgs
}

function claude {
    _xmlui_mark_agent claude
    _xmlui_run_real claude $args
}

function codex {
    _xmlui_mark_agent codex
    _xmlui_run_real codex $args
}
