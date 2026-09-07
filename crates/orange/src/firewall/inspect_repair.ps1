# Orange firewall inspection/repair helper.
#
# Baked into orange.exe with `include_str!` and fed to `powershell.exe -Command -`
# over stdin (see firewall.rs). Never written to disk in production: the file
# here exists so the child process source is reviewable/versioned/diffable.
#
# Facts are read primarily through the `HNetCfg.FwPolicy2` COM object, not the
# `NetSecurity` CIM cmdlets: `Get-NetFirewallRule` objects have no `Protocol`
# property at all (it lives on a separate, per-rule `Get-NetFirewallPortFilter`
# association) and `-Set-StrictMode -Version Latest` turns reading one into a
# terminating error; `-All` is also its own parameter set that cannot be
# combined with `-Direction`/`-Enabled`/`-Action` (confirmed against a live
# `Get-Command Get-NetFirewallRule).ParameterSets`). COM's `INetFwRule` has
# every scoping property as a plain, always-present field, so there is nothing
# to "come back missing" the way a CIM associated filter can. CIM is used for
# exactly one thing COM cannot answer: whether a rule matching this exact path
# is locally owned or Group-Policy-sourced (`PolicyStoreSourceType`), via the
# same `-Program` application-filter association used for mutation, which is
# already exact-path-scoped by the OS itself before any of our own filtering.
#
# Ownership rule: never mutate anything outside a rule whose ApplicationName
# is exactly $env:ORANGE_FW_EXE, and never widen a network profile the current
# machine does not have active right now.
#
# Everything that can fail is fail-closed: a query error, a scan that could
# not prove a rule unrestricted, or an inability to determine the active
# profile all become the fixed `error` output, which the Rust caller turns
# into an Unknown result rather than a false Clear or a false Blocked.
#
# Inputs arrive as environment variables set by the direct parent process
# (never string-interpolated into this script text), because this script is
# spawned by our own Rust code as a normal child, not through an elevation hop:
#   ORANGE_FW_ACTION         "Inspect" or "Repair"
#   ORANGE_FW_EXE            the exact, already-canonicalized path to orange.exe
#   ORANGE_FW_REQUESTER_PID  (Repair only) the PID that asked for this repair;
#                            checked again immediately before each mutating
#                            call, in addition to Rust's own held-handle check
#                            before this script is even started
#   ORANGE_FW_SYSTEM32       the trusted System32 directory (from
#                            GetSystemDirectoryW, not the SystemRoot env var),
#                            used to load NetSecurity from a known path and to
#                            replace PSModulePath before importing anything
#
# Output is exactly one line of compact JSON on stdout. Any failure still
# tries to emit `{"schema":2,"error":"..."}` so the caller gets a classifiable
# result instead of a bare non-zero exit code. Nothing here ever prints a rule
# name, a raw path other than the exact one it was given, or a user identity.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
# Rule paths can contain non-ASCII usernames. The Rust boundary reads UTF-8;
# the inherited console code page is not a stable serialization encoding.
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

# NET_FW_PROFILE_TYPE2_ bit values (unchanged across Windows Firewall COM
# versions): Domain=1, Private=2, Public=4.
$Script:OrangeProfileBits = [ordered]@{ Domain = 1; Private = 2; Public = 4 }

function Write-Facts {
    param($Facts)
    # -Compress keeps this well under the 16 KiB the caller bounds stdout to;
    # the facts payload is at most a few dozen small records.
    $Facts | ConvertTo-Json -Compress -Depth 6
}

function Write-FailureAndExit {
    param([string]$Reason)
    @{ schema = 2; error = $Reason } | ConvertTo-Json -Compress
    exit 1
}

function Import-OrangeNetSecurityModule {
    # Extracted into its own function so a test harness can redefine just this
    # one function to a no-op and supply mock `*-NetFirewall*` cmdlets instead
    # - no test-only flag is read by production code anywhere in this file.
    param([string]$System32)
    if ([string]::IsNullOrWhiteSpace($System32)) {
        throw 'missing-system32'
    }
    # Replace PSModulePath entirely (rather than prepending) so a user-writable
    # directory earlier on a normal PSModulePath can never shadow the module
    # or the commands it exports, even under module autoloading.
    $env:PSModulePath = Join-Path $System32 'WindowsPowerShell\v1.0\Modules'
    $modulePath = Join-Path $System32 'WindowsPowerShell\v1.0\Modules\NetSecurity\NetSecurity.psd1'
    Import-Module $modulePath -ErrorAction Stop -Force
}

function New-OrangeFirewallPolicy {
    # Extracted into its own function, alongside `Import-OrangeNetSecurityModule`,
    # so a test harness can override just this one function with a fake object
    # shaped like the COM interface instead of touching the real Windows
    # Firewall policy store. No test-only flag is read by production code.
    return New-Object -ComObject HNetCfg.FwPolicy2
}

function Get-OrangeProfileNames {
    param([int]$Bitmask)
    $names = New-Object System.Collections.Generic.List[string]
    foreach ($name in $Script:OrangeProfileBits.Keys) {
        if ($Bitmask -band $Script:OrangeProfileBits[$name]) { $names.Add($name) }
    }
    return $names
}

function Get-OrangeActiveProfiles {
    param($Fw)
    # `CurrentProfileTypes` is the same value the Firewall control panel and
    # `netsh advfirewall` use for "what network is this". A machine with
    # adapters on different networks can have more than one bit set. Zero (or
    # an unrecognized value) is a real failure, not "assume Public": a repair
    # must never guess which profile it is allowed to touch.
    $current = [int]$Fw.CurrentProfileTypes
    if ($current -le 0) { throw 'no-active-profile' }
    $knownMask = 0
    foreach ($bit in $Script:OrangeProfileBits.Values) { $knownMask = $knownMask -bor [int]$bit }
    if (($current -band (-bnot $knownMask)) -ne 0) { throw 'unrecognized-active-profile' }
    $names = @(Get-OrangeProfileNames -Bitmask $current)
    if ($names.Count -eq 0) { throw 'unrecognized-active-profile' }
    return $names
}

function Get-OrangeProfileFacts {
    param($Fw)
    $result = @()
    foreach ($name in $Script:OrangeProfileBits.Keys) {
        $bit = $Script:OrangeProfileBits[$name]
        # NET_FW_ACTION_BLOCK = 0, NET_FW_ACTION_ALLOW = 1 (INetFwPolicy2).
        $outboundBlock = ([int]$Fw.DefaultOutboundAction($bit)) -eq 0
        $result += [ordered]@{
            name            = $name
            enabled         = [bool]$Fw.FirewallEnabled($bit)
            blockAllInbound = [bool]$Fw.BlockAllInboundTraffic($bit)
            outboundBlock   = [bool]$outboundBlock
        }
    }
    return $result
}

function Test-OrangeRuleWideOpen {
    param($Rule)
    # Fail closed: anything other than the exact "no scope at all" sentinel on
    # every one of these dimensions counts as restricted, and a property this
    # throws on reading is treated the same as a restriction, never as "Any".
    ([string]$Rule.LocalAddresses) -eq '*' -and
        ([string]$Rule.RemoteAddresses) -eq '*' -and
        ([string]$Rule.LocalPorts) -eq '*' -and
        ([string]$Rule.RemotePorts) -eq '*' -and
        ([string]$Rule.InterfaceTypes).Equals('All', [System.StringComparison]::OrdinalIgnoreCase) -and
        (@($Rule.Interfaces).Count) -eq 0 -and
        [string]::IsNullOrEmpty([string]$Rule.serviceName) -and
        [string]::IsNullOrEmpty([string]$Rule.LocalAppPackageId) -and
        [string]::IsNullOrEmpty([string]$Rule.LocalUserOwner) -and
        [string]::IsNullOrEmpty([string]$Rule.LocalUserAuthorizedList) -and
        [string]::IsNullOrEmpty([string]$Rule.RemoteUserAuthorizedList) -and
        [string]::IsNullOrEmpty([string]$Rule.RemoteMachineAuthorizedList) -and
        [string]::IsNullOrEmpty([string]$Rule.IcmpTypesAndCodes) -and
        ([int]$Rule.SecureFlags) -eq 0
}

function Test-OrangeRuleRestricted {
    param($Rule)
    try {
        return -not (Test-OrangeRuleWideOpen -Rule $Rule)
    } catch {
        return $true
    }
}

function ConvertTo-OrangeProtocolName {
    param([int]$Code)
    switch ($Code) {
        6 { 'TCP' }
        17 { 'UDP' }
        256 { 'Any' }
        default { 'Other' }
    }
}

function Get-OrangeAppRuleFacts {
    param($Fw, [string]$Exe)
    $result = @()
    $count = 0
    foreach ($rule in $Fw.Rules) {
        # Any property access below throwing for a rule that already matched
        # our exact path fails the whole scan rather than silently skipping
        # it: an unreadable rule is exactly the case that must never resolve
        # to Clear.
        $program = [string]$rule.ApplicationName
        if ([string]::IsNullOrWhiteSpace($program)) { continue }
        if (-not $program.Trim().Equals($Exe.Trim(), [System.StringComparison]::OrdinalIgnoreCase)) {
            continue
        }
        $direction = [int]$rule.Direction
        if ($direction -ne 1) { continue } # inbound only; see module doc comment
        if ($count -ge 32) { throw 'scan-limit' }
        $result += [ordered]@{
            program    = $program
            enabled    = [bool]$rule.Enabled
            action     = if (([int]$rule.Action) -eq 0) { 'Block' } else { 'Allow' }
            protocol   = ConvertTo-OrangeProtocolName -Code ([int]$rule.Protocol)
            profiles   = @(Get-OrangeProfileNames -Bitmask ([int]$rule.Profiles))
            restricted = [bool](Test-OrangeRuleRestricted -Rule $rule)
        }
        $count += 1
    }
    return $result
}

function Test-OrangeComManagedOrigin {
    param($Fw)
    try {
        $state = [int]$Fw.LocalPolicyModifyState
        return $state -ne 0
    } catch {
        # If this COM guard cannot be read at all, fail closed on repairability
        # by treating origin as managed.
        return $true
    }
}

function Test-OrangeAnyManagedOrigin {
    param([string]$Exe, [string[]]$ActiveProfiles)
    # COM cannot say whether a rule is locally owned or Group-Policy-sourced
    # (see module doc comment), so this is the one place CIM is still used for
    # reading. `-Program` with nothing to find is a real "not found" condition
    # on this cmdlet, not evidence of anything - only a *different* kind of
    # failure (service unavailable, access denied, ...) should fail the scan.
    # Scoped to exactly the same predicate as the COM-based "matching" filter
    # in Rust (`classify_facts`): enabled, inbound, Block, active profile -
    # anything else (an unrelated Allow rule, or a Block rule that only
    # applies to a profile that is not active right now) must not force a
    # repairable local block to be reported as managed.
    try {
        $filters = Get-NetFirewallApplicationFilter -Program $Exe -PolicyStore ActiveStore -ErrorAction Stop
    } catch {
        if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { return $false }
        throw
    }
    foreach ($filter in @($filters)) {
        $rules = @($filter | Get-NetFirewallRule -PolicyStore ActiveStore -TracePolicyStore -ErrorAction Stop)
        foreach ($r in $rules) {
            if (([string]$r.Direction) -ne 'Inbound' -or ([string]$r.Action) -ne 'Block') { continue }
            if ($r.Enabled -ne 'True' -and $r.Enabled -ne $true) { continue }
            $ruleProfiles = @(Get-OrangeProfileNames -Bitmask ([int]$r.Profile))
            if (-not ($ruleProfiles | Where-Object { $_ -in $ActiveProfiles })) { continue }
            if (([string]$r.PolicyStoreSourceType) -ne 'Local') { return $true }
        }
    }
    return $false
}

function Get-OrangeFacts {
    param($Fw, [string]$Exe)
    $active = @(Get-OrangeActiveProfiles -Fw $Fw)
    return [ordered]@{
        schema         = 2
        activeProfiles = $active
        profiles       = @(Get-OrangeProfileFacts -Fw $Fw)
        appRules       = @(Get-OrangeAppRuleFacts -Fw $Fw -Exe $Exe)
        managedOrigin  = [bool]((Test-OrangeComManagedOrigin -Fw $Fw) -or (Test-OrangeAnyManagedOrigin -Exe $Exe -ActiveProfiles $active))
    }
}

function Test-OrangeRequesterAlive {
    param([string]$RequesterPid)
    if ([string]::IsNullOrWhiteSpace($RequesterPid)) { return $false }
    $parsed = 0
    if (-not [int]::TryParse($RequesterPid, [ref]$parsed) -or $parsed -le 0) { return $false }
    # Best-effort, PID-based secondary check for the duration of *this*
    # script only: Rust already verified liveness through a held process
    # HANDLE (immune to PID reuse) immediately before starting this process,
    # and does so again on its own before trusting the result. This check
    # exists only to stop a mutation loop early if the requester happens to
    # exit while the script itself is still running.
    return $null -ne (Get-Process -Id $parsed -ErrorAction SilentlyContinue)
}

function New-OrangeRuleName {
    param([string]$Exe, [string]$Transport)
    # A deterministic name derived only from the exact path means a second run
    # finds the same rule (idempotent) and a different path can never collide
    # with, or overwrite, an unrelated rule.
    $sha256 = [System.Security.Cryptography.SHA256]::Create()
    try {
        $bytes = [System.Text.Encoding]::UTF8.GetBytes($Exe.ToLowerInvariant())
        $hash = [System.BitConverter]::ToString($sha256.ComputeHash($bytes)).Replace('-', '').Substring(0, 16)
    } finally {
        $sha256.Dispose()
    }
    return "OrangeMediaInbound-$Transport-$hash"
}

function Get-OrangeLocalAppRules {
    param([string]$Exe)
    # Same exact-path association pattern as `Test-OrangeAnyManagedOrigin`,
    # against the mutable store instead of the effective one, and tolerant of
    # the same benign "nothing found" condition.
    try {
        $filters = Get-NetFirewallApplicationFilter -Program $Exe -PolicyStore PersistentStore -ErrorAction Stop
    } catch {
        if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { return @() }
        throw
    }
    $rules = @()
    foreach ($filter in @($filters)) {
        $rules += @($filter | Get-NetFirewallRule -PolicyStore PersistentStore -ErrorAction Stop)
    }
    return $rules
}

function Test-OrangeAnyScopeValue {
    param($Value)
    if ($null -eq $Value) { return $true }
    foreach ($item in @($Value)) {
        $text = [string]$item
        if ([string]::IsNullOrWhiteSpace($text)) { continue }
        if ($text.Equals('Any', [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        if ($text.Equals('All', [System.StringComparison]::OrdinalIgnoreCase)) { continue }
        if ($text -eq '*') { continue }
        return $false
    }
    return $true
}

function Test-OrangeSecurityFilterWideOpen {
    param($SecurityFilter)
    if ($null -eq $SecurityFilter) { return $true }
    if (-not (Test-OrangeAnyScopeValue -Value $SecurityFilter.LocalUser)) { return $false }
    if (-not (Test-OrangeAnyScopeValue -Value $SecurityFilter.RemoteUser)) { return $false }
    if (-not (Test-OrangeAnyScopeValue -Value $SecurityFilter.RemoteMachine)) { return $false }
    $auth = [string]$SecurityFilter.Authentication
    if (-not [string]::IsNullOrWhiteSpace($auth) -and -not $auth.Equals('NotRequired', [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    $encryption = [string]$SecurityFilter.Encryption
    if (-not [string]::IsNullOrWhiteSpace($encryption) -and -not $encryption.Equals('NotRequired', [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    if (($SecurityFilter.OverrideBlockRules -eq $true) -or ([string]$SecurityFilter.OverrideBlockRules).Equals('True', [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    return $true
}

function Test-OrangeCimRuleWholeAppUnrestricted {
    param($Rule, [string]$Exe)
    if (([string]$Rule.Direction) -ne 'Inbound') { return $false }
    if (([string]$Rule.PolicyStoreSourceType) -ne 'Local') { return $false }
    $applicationFilters = @($Rule | Get-NetFirewallApplicationFilter -PolicyStore PersistentStore -ErrorAction Stop)
    if ($applicationFilters.Count -ne 1) { return $false }
    $appFilter = $applicationFilters[0]
    $program = [string]$appFilter.Program
    if ([string]::IsNullOrWhiteSpace($program)) { return $false }
    if (-not $program.Equals($Exe, [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    if (-not (Test-OrangeAnyScopeValue -Value $appFilter.Package)) { return $false }
    if (-not (Test-OrangeAnyScopeValue -Value $Rule.LocalUserOwner)) { return $false }

    $portFilter = $Rule | Get-NetFirewallPortFilter -PolicyStore PersistentStore -ErrorAction Stop
    $protocol = [string]$portFilter.Protocol
    if ($protocol -notin @('TCP', 'UDP', 'Any', '6', '17', '256')) { return $false }
    if (([string]$portFilter.LocalPort) -ne 'Any' -or ([string]$portFilter.RemotePort) -ne 'Any') { return $false }
    $addressFilter = $Rule | Get-NetFirewallAddressFilter -PolicyStore PersistentStore -ErrorAction Stop
    if (([string]$addressFilter.LocalAddress) -ne 'Any' -or ([string]$addressFilter.RemoteAddress) -ne 'Any') { return $false }
    $serviceFilter = $Rule | Get-NetFirewallServiceFilter -PolicyStore PersistentStore -ErrorAction Stop
    if (-not (Test-OrangeAnyScopeValue -Value $serviceFilter.Service)) { return $false }
    $interfaceTypeFilter = $Rule | Get-NetFirewallInterfaceTypeFilter -PolicyStore PersistentStore -ErrorAction Stop
    if (-not (Test-OrangeAnyScopeValue -Value $interfaceTypeFilter.InterfaceType)) { return $false }
    $interfaceFilter = $Rule | Get-NetFirewallInterfaceFilter -PolicyStore PersistentStore -ErrorAction Stop
    if (-not (Test-OrangeAnyScopeValue -Value $interfaceFilter.InterfaceAlias)) { return $false }
    $securityFilter = $Rule | Get-NetFirewallSecurityFilter -PolicyStore PersistentStore -ErrorAction Stop
    if (-not (Test-OrangeSecurityFilterWideOpen -SecurityFilter $securityFilter)) { return $false }
    if (-not [string]::IsNullOrEmpty([string]$Rule.PackageFamilyName)) { return $false }
    return $true
}

function Test-OrangeCimRuleEligibleForDisable {
    param($Rule, [string]$Exe)
    # The final, immediately-before-mutation eligibility check: re-derives
    # local origin, restriction and direction/action/protocol from the CIM
    # object about to be mutated, rather than trusting facts collected a
    # moment earlier by the COM-based scan.
    if (([string]$Rule.Action) -ne 'Block') { return $false }
    if ($Rule.Enabled -ne 'True' -and $Rule.Enabled -ne $true) { return $false }
    return Test-OrangeCimRuleWholeAppUnrestricted -Rule $Rule -Exe $Exe
}

function Test-OrangeProfileSetEqual {
    param([string[]]$Left, [string[]]$Right)
    $leftNorm = @($Left | Sort-Object -Unique)
    $rightNorm = @($Right | Sort-Object -Unique)
    if ($leftNorm.Count -ne $rightNorm.Count) { return $false }
    for ($i = 0; $i -lt $leftNorm.Count; $i++) {
        if (-not $leftNorm[$i].Equals($rightNorm[$i], [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    }
    return $true
}

function Assert-OrangeRepairGuardsStable {
    param($Fw, [string]$Exe, [string[]]$InitialActiveProfiles)
    $currentActive = @(Get-OrangeActiveProfiles -Fw $Fw)
    if (-not (Test-OrangeProfileSetEqual -Left $currentActive -Right $InitialActiveProfiles)) {
        throw 'profile-changed'
    }
    foreach ($profileFact in @(Get-OrangeProfileFacts -Fw $Fw)) {
        if ($profileFact.name -notin $InitialActiveProfiles) { continue }
        if ($profileFact.blockAllInbound -or $profileFact.outboundBlock) {
            throw 'policy-changed'
        }
    }
    if (Test-OrangeComManagedOrigin -Fw $Fw) { throw 'policy-changed' }
    if (Test-OrangeAnyManagedOrigin -Exe $Exe -ActiveProfiles $InitialActiveProfiles) { throw 'policy-changed' }
}

function Get-OrangeNamedRuleOrNull {
    param([string]$Name)
    try {
        return Get-NetFirewallRule -Name $Name -PolicyStore PersistentStore -ErrorAction Stop
    } catch {
        if ($_.CategoryInfo.Category -eq 'ObjectNotFound') { return $null }
        throw
    }
}

function Test-OrangeNamedAllowRuleCompatible {
    param($Rule, [string]$Exe, [string]$Transport)
    if (-not (Test-OrangeCimRuleWholeAppUnrestricted -Rule $Rule -Exe $Exe)) { return $false }
    if (([string]$Rule.Action) -ne 'Allow') { return $false }
    $portFilter = $Rule | Get-NetFirewallPortFilter -PolicyStore PersistentStore -ErrorAction Stop
    $protocol = [string]$portFilter.Protocol
    if ($protocol.Equals('6', [System.StringComparison]::OrdinalIgnoreCase)) { $protocol = 'TCP' }
    if ($protocol.Equals('17', [System.StringComparison]::OrdinalIgnoreCase)) { $protocol = 'UDP' }
    if (-not $protocol.Equals($Transport, [System.StringComparison]::OrdinalIgnoreCase)) { return $false }
    return $true
}

function Assert-OrangeAllowRuleTargetsCompatible {
    param([string]$Exe)
    foreach ($transport in @('TCP', 'UDP')) {
        $name = New-OrangeRuleName -Exe $Exe -Transport $transport
        $existing = Get-OrangeNamedRuleOrNull -Name $name
        if (-not $existing) { continue }
        if (-not (Test-OrangeNamedAllowRuleCompatible -Rule $existing -Exe $Exe -Transport $transport)) {
            throw 'allow-rule-collision'
        }
    }
}

function Repair-OrangeAppBlocks {
    param($Fw, [string]$Exe, [string[]]$ActiveProfiles, [string]$RequesterPid)
    $localRules = @(Get-OrangeLocalAppRules -Exe $Exe)
    foreach ($rule in $localRules) {
        if (-not (Test-OrangeRequesterAlive -RequesterPid $RequesterPid)) { return $true }
        try {
            $eligible = Test-OrangeCimRuleEligibleForDisable -Rule $rule -Exe $Exe
        } catch {
            $eligible = $false
        }
        if (-not $eligible) { continue }

        $ruleProfiles = @(Get-OrangeProfileNames -Bitmask ([int]$rule.Profile))
        if (-not ($ruleProfiles | Where-Object { $_ -in $ActiveProfiles })) { continue }
        $remainder = @($ruleProfiles | Where-Object { $_ -notin $ActiveProfiles })
        if (-not (Test-OrangeRequesterAlive -RequesterPid $RequesterPid)) { return $true }
        if ($remainder.Count -eq 0) {
            Assert-OrangeRepairGuardsStable -Fw $Fw -Exe $Exe -InitialActiveProfiles $ActiveProfiles
            Disable-NetFirewallRule -InputObject $rule -ErrorAction Stop
        } else {
            Assert-OrangeRepairGuardsStable -Fw $Fw -Exe $Exe -InitialActiveProfiles $ActiveProfiles
            Set-NetFirewallRule -InputObject $rule -Profile ($remainder -join ',') -ErrorAction Stop
        }
    }
    return $false
}

function Add-OrangeAppAllow {
    param($Fw, [string]$Exe, [string[]]$ActiveProfiles, [string]$RequesterPid)
    foreach ($transport in @('TCP', 'UDP')) {
        if (-not (Test-OrangeRequesterAlive -RequesterPid $RequesterPid)) { return $true }
        $name = New-OrangeRuleName -Exe $Exe -Transport $transport
        $existing = Get-OrangeNamedRuleOrNull -Name $name
        if (-not $existing) {
            if (-not (Test-OrangeRequesterAlive -RequesterPid $RequesterPid)) { return $true }
            Assert-OrangeRepairGuardsStable -Fw $Fw -Exe $Exe -InitialActiveProfiles $ActiveProfiles
            New-NetFirewallRule -Name $name -DisplayName "Orange Media ($transport)" `
                -Direction Inbound -Action Allow -Protocol $transport -Program $Exe `
                -Profile ($ActiveProfiles -join ',') -PolicyStore PersistentStore -Enabled True | Out-Null
            continue
        }
        if (-not (Test-OrangeNamedAllowRuleCompatible -Rule $existing -Exe $Exe -Transport $transport)) {
            throw 'allow-rule-collision'
        }

        $wasEnabled = ($existing.Enabled -eq 'True' -or $existing.Enabled -eq $true)
        $existingProfiles = @(Get-OrangeProfileNames -Bitmask ([int]$existing.Profile))
        # A disabled rule's old profile scope was never active-verified by
        # this run: reactivating it must not silently resurrect scope beyond
        # what is active right now. An already-enabled rule only ever widens.
        $targetProfiles = if ($wasEnabled) {
            @(($existingProfiles + $ActiveProfiles) | Select-Object -Unique)
        } else {
            @($ActiveProfiles | Select-Object -Unique)
        }
        $needsUpdate = (-not $wasEnabled) -or
            (([string]$existing.Action) -ne 'Allow') -or
            (@($targetProfiles | Where-Object { $_ -notin $existingProfiles }).Count -gt 0)
        if ($needsUpdate) {
            if (-not (Test-OrangeRequesterAlive -RequesterPid $RequesterPid)) { return $true }
            Assert-OrangeRepairGuardsStable -Fw $Fw -Exe $Exe -InitialActiveProfiles $ActiveProfiles
            Set-NetFirewallRule -InputObject $existing -Enabled True -Action Allow `
                -Profile ($targetProfiles -join ',') -ErrorAction Stop
        }
    }
    return $false
}

function Invoke-OrangeRepair {
    param($Fw, [string]$Exe, [string]$RequesterPid)
    if (-not (Test-OrangeRequesterAlive -RequesterPid $RequesterPid)) {
        return [ordered]@{ schema = 2; requesterCancelled = $true }
    }
    $active = @(Get-OrangeActiveProfiles -Fw $Fw)
    Assert-OrangeAllowRuleTargetsCompatible -Exe $Exe
    Assert-OrangeRepairGuardsStable -Fw $Fw -Exe $Exe -InitialActiveProfiles $active
    $cancelled = Repair-OrangeAppBlocks -Fw $Fw -Exe $Exe -ActiveProfiles $active -RequesterPid $RequesterPid
    if (-not $cancelled) {
        $cancelled = Add-OrangeAppAllow -Fw $Fw -Exe $Exe -ActiveProfiles $active -RequesterPid $RequesterPid
    }
    if ($cancelled) {
        return [ordered]@{ schema = 2; requesterCancelled = $true }
    }
    return Get-OrangeFacts -Fw $Fw -Exe $Exe
}

function Invoke-OrangeFirewallAction {
    try {
        $action = $env:ORANGE_FW_ACTION
        $exe = $env:ORANGE_FW_EXE
        if ([string]::IsNullOrWhiteSpace($action) -or [string]::IsNullOrWhiteSpace($exe)) {
            Write-FailureAndExit 'missing-arguments'
        }

        Import-OrangeNetSecurityModule -System32 $env:ORANGE_FW_SYSTEM32
        $fw = New-OrangeFirewallPolicy

        switch ($action) {
            'Inspect' {
                Write-Facts (Get-OrangeFacts -Fw $fw -Exe $exe)
            }
            'Repair' {
                $requesterPid = $env:ORANGE_FW_REQUESTER_PID
                Write-Facts (Invoke-OrangeRepair -Fw $fw -Exe $exe -RequesterPid $requesterPid)
            }
            default {
                Write-FailureAndExit 'unknown-action'
            }
        }
    } catch {
        $known = @('missing-arguments', 'unknown-action', 'scan-limit', 'allow-rule-collision', 'profile-changed', 'policy-changed')
        $reason = [string]$_.Exception.Message
        if ($reason -in $known) {
            Write-FailureAndExit $reason
        }
        Write-FailureAndExit 'exception'
    }
}

Invoke-OrangeFirewallAction
