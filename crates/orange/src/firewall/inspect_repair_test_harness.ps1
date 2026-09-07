# Test-only harness for `inspect_repair.ps1`'s PowerShell functions.

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$productionPath = Join-Path $PSScriptRoot 'inspect_repair.ps1'
$productionText = Get-Content -Raw -Path $productionPath
$cutIndex = $productionText.LastIndexOf('Invoke-OrangeFirewallAction')
if ($cutIndex -lt 0) { throw 'could not find production entry point call to strip' }
Invoke-Expression ($productionText.Substring(0, $cutIndex))

function Import-OrangeNetSecurityModule { param([string]$System32) }

$Script:MockFwPolicy = $null
function New-OrangeFirewallPolicy { return $Script:MockFwPolicy }

function New-OrangeMockFwPolicy {
    param(
        [int]$CurrentProfileTypes = 4,
        [int]$LocalPolicyModifyState = 0,
        [hashtable]$EnabledByProfile = @{ 1 = $true; 2 = $true; 4 = $true },
        [hashtable]$BlockAllInboundByProfile = @{ 1 = $false; 2 = $false; 4 = $false },
        [hashtable]$OutboundBlockByProfile = @{ 1 = $false; 2 = $false; 4 = $false },
        [object[]]$Rules = @()
    )
    $policy = [pscustomobject]@{
        CurrentProfileTypes   = $CurrentProfileTypes
        LocalPolicyModifyState = $LocalPolicyModifyState
        Rules                 = $Rules
    }
    $policy | Add-Member -MemberType ScriptMethod -Name FirewallEnabled -Value {
        param($ProfileType) $EnabledByProfile[[int]$ProfileType]
    }.GetNewClosure()
    $policy | Add-Member -MemberType ScriptMethod -Name BlockAllInboundTraffic -Value {
        param($ProfileType) $BlockAllInboundByProfile[[int]$ProfileType]
    }.GetNewClosure()
    $policy | Add-Member -MemberType ScriptMethod -Name DefaultOutboundAction -Value {
        param($ProfileType) if ($OutboundBlockByProfile[[int]$ProfileType]) { 0 } else { 1 }
    }.GetNewClosure()
    return $policy
}

function New-OrangeMockComRule {
    param(
        [string]$ApplicationName,
        [int]$Direction = 1,
        [int]$Action = 0,
        [bool]$Enabled = $true,
        [int]$Protocol = 6,
        [int]$Profiles = 4,
        [string]$LocalAddresses = '*',
        [string]$RemoteAddresses = '*',
        [string]$LocalPorts = '*',
        [string]$RemotePorts = '*',
        [string]$InterfaceTypes = 'All',
        [object[]]$Interfaces = @(),
        [string]$ServiceName = '',
        [string]$LocalAppPackageId = '',
        [string]$LocalUserOwner = '',
        [string]$LocalUserAuthorizedList = '',
        [string]$RemoteUserAuthorizedList = '',
        [string]$RemoteMachineAuthorizedList = '',
        [string]$IcmpTypesAndCodes = '',
        [int]$SecureFlags = 0,
        [switch]$ThrowOnRestrictionRead
    )
    $rule = [pscustomobject]@{
        ApplicationName             = $ApplicationName
        Direction                   = $Direction
        Action                      = $Action
        Enabled                     = $Enabled
        Protocol                    = $Protocol
        Profiles                    = $Profiles
        LocalAddresses              = $LocalAddresses
        RemoteAddresses             = $RemoteAddresses
        LocalPorts                  = $LocalPorts
        RemotePorts                 = $RemotePorts
        InterfaceTypes              = $InterfaceTypes
        Interfaces                  = $Interfaces
        serviceName                 = $ServiceName
        LocalAppPackageId           = $LocalAppPackageId
        LocalUserOwner              = $LocalUserOwner
        LocalUserAuthorizedList     = $LocalUserAuthorizedList
        RemoteUserAuthorizedList    = $RemoteUserAuthorizedList
        RemoteMachineAuthorizedList = $RemoteMachineAuthorizedList
        IcmpTypesAndCodes           = $IcmpTypesAndCodes
        SecureFlags                 = $SecureFlags
    }
    if ($ThrowOnRestrictionRead) {
        $rule | Add-Member -MemberType ScriptProperty -Name LocalAddresses -Value {
            throw 'simulated property read failure'
        } -Force
    }
    return $rule
}

$Script:MockCimAppFilters = @{}
$Script:MockCimRulesByFilter = @{}
$Script:MockCimRulesByName = @{}
$Script:MockCallLog = New-Object System.Collections.Generic.List[string]
$Script:MockAliveRequesterPid = $null
$Script:MockGetProcessCallCount = 0
$Script:MockOnGetProcess = $null

function New-OrangeMockCimRule {
    param(
        [string]$Name,
        [string]$Program,
        [string]$Direction = 'Inbound',
        [string]$Action = 'Block',
        $Enabled = $true,
        [int]$Profile = 4,
        [string]$PolicyStoreSourceType = 'Local',
        [string]$Protocol = 'TCP',
        [string]$LocalPort = 'Any',
        [string]$RemotePort = 'Any',
        [string]$LocalAddress = 'Any',
        [string]$RemoteAddress = 'Any',
        [string]$Service = 'Any',
        [string[]]$InterfaceType = @('All'),
        [string[]]$InterfaceAlias = @('Any'),
        [string]$PackageFamilyName = '',
        [string]$LocalUserOwner = '',
        [object[]]$ApplicationFilters = $null,
        [string]$ApplicationFilterPackage = 'Any',
        [switch]$ApplicationFilterUnreadable,
        [string]$SecurityAuthentication = 'NotRequired',
        [string]$SecurityEncryption = 'NotRequired',
        [bool]$SecurityOverrideBlockRules = $false,
        [string]$SecurityLocalUser = 'Any',
        [string]$SecurityRemoteUser = 'Any',
        [string]$SecurityRemoteMachine = 'Any'
    )
    if ($null -eq $ApplicationFilters) {
        $ApplicationFilters = @([pscustomobject]@{ Program = $Program; Package = $ApplicationFilterPackage })
    }
    $rule = [pscustomobject]@{
        Name                        = $Name
        Program                     = $Program
        Direction                   = $Direction
        Action                      = $Action
        Enabled                     = $Enabled
        Profile                     = $Profile
        PolicyStoreSourceType       = $PolicyStoreSourceType
        Protocol                    = $Protocol
        LocalPort                   = $LocalPort
        RemotePort                  = $RemotePort
        LocalAddress                = $LocalAddress
        RemoteAddress               = $RemoteAddress
        Service                     = $Service
        InterfaceType               = $InterfaceType
        InterfaceAlias              = $InterfaceAlias
        PackageFamilyName           = $PackageFamilyName
        LocalUserOwner              = $LocalUserOwner
        ApplicationFilters          = $ApplicationFilters
        ApplicationFilterUnreadable = [bool]$ApplicationFilterUnreadable
        SecurityAuthentication      = $SecurityAuthentication
        SecurityEncryption          = $SecurityEncryption
        SecurityOverrideBlockRules  = $SecurityOverrideBlockRules
        SecurityLocalUser           = $SecurityLocalUser
        SecurityRemoteUser          = $SecurityRemoteUser
        SecurityRemoteMachine       = $SecurityRemoteMachine
    }
    $Script:MockCimRulesByName[$Name] = $rule
    return $rule
}

function Register-OrangeMockCimRuleForExe {
    param([string]$Exe, $Rule)
    $key = $Exe.ToLowerInvariant()
    if (-not $Script:MockCimAppFilters.ContainsKey($key)) {
        $Script:MockCimAppFilters[$key] = @([pscustomobject]@{ Program = $Exe; Id = $key })
    }
    if (-not $Script:MockCimRulesByFilter.ContainsKey($key)) {
        $Script:MockCimRulesByFilter[$key] = @()
    }
    $Script:MockCimRulesByFilter[$key] += $Rule
}

function New-OrangeObjectNotFoundError {
    param([string]$Message, $Target)
    return New-Object System.Management.Automation.ErrorRecord(
        (New-Object System.Exception($Message)),
        'CmdletizationQuery_NotFound',
        [System.Management.Automation.ErrorCategory]::ObjectNotFound,
        $Target
    )
}

function Get-NetFirewallApplicationFilter {
    param([string]$Program, [string]$PolicyStore, [Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        if ($InputObject) {
            $rule = $Script:MockCimRulesByName[$InputObject.Name]
            if (-not $rule) { return $null }
            if ($rule.ApplicationFilterUnreadable) { throw 'simulated-application-filter-read-failure' }
            return @($rule.ApplicationFilters)
        }
        $key = $Program.ToLowerInvariant()
        if ($Script:MockCimAppFilters.ContainsKey($key)) {
            return $Script:MockCimAppFilters[$key]
        }
        throw (New-OrangeObjectNotFoundError -Message "No application filter for '$Program'." -Target $Program)
    }
}

function Get-NetFirewallRule {
    param(
        [Parameter(ValueFromPipeline = $true)]$InputObject,
        [string]$Name,
        [string]$PolicyStore,
        [switch]$TracePolicyStore
    )
    process {
        if ($InputObject -and $InputObject.Id) {
            return @($Script:MockCimRulesByFilter[$InputObject.Id])
        }
        if ($Name) {
            $rule = $Script:MockCimRulesByName[$Name]
            if ($rule) { return $rule }
            return $null
        }
        return @()
    }
}

function Get-NetFirewallPortFilter {
    param([Parameter(ValueFromPipeline = $true)]$AssociatedNetFirewallRule, [string]$PolicyStore)
    process {
        [pscustomobject]@{
            Protocol   = $AssociatedNetFirewallRule.Protocol
            LocalPort  = $AssociatedNetFirewallRule.LocalPort
            RemotePort = $AssociatedNetFirewallRule.RemotePort
        }
    }
}

function Get-NetFirewallAddressFilter {
    param([Parameter(ValueFromPipeline = $true)]$AssociatedNetFirewallRule, [string]$PolicyStore)
    process {
        [pscustomobject]@{
            LocalAddress  = $AssociatedNetFirewallRule.LocalAddress
            RemoteAddress = $AssociatedNetFirewallRule.RemoteAddress
        }
    }
}

function Get-NetFirewallServiceFilter {
    param([Parameter(ValueFromPipeline = $true)]$AssociatedNetFirewallRule, [string]$PolicyStore)
    process { [pscustomobject]@{ Service = $AssociatedNetFirewallRule.Service } }
}

function Get-NetFirewallInterfaceTypeFilter {
    param([Parameter(ValueFromPipeline = $true)]$AssociatedNetFirewallRule, [string]$PolicyStore)
    process { [pscustomobject]@{ InterfaceType = @($AssociatedNetFirewallRule.InterfaceType) } }
}

function Get-NetFirewallInterfaceFilter {
    param([Parameter(ValueFromPipeline = $true)]$AssociatedNetFirewallRule, [string]$PolicyStore)
    process { [pscustomobject]@{ InterfaceAlias = @($AssociatedNetFirewallRule.InterfaceAlias) } }
}

function Get-NetFirewallSecurityFilter {
    param([Parameter(ValueFromPipeline = $true)]$AssociatedNetFirewallRule, [string]$PolicyStore)
    process {
        [pscustomobject]@{
            Authentication    = $AssociatedNetFirewallRule.SecurityAuthentication
            Encryption        = $AssociatedNetFirewallRule.SecurityEncryption
            OverrideBlockRules = $AssociatedNetFirewallRule.SecurityOverrideBlockRules
            LocalUser         = $AssociatedNetFirewallRule.SecurityLocalUser
            RemoteUser        = $AssociatedNetFirewallRule.SecurityRemoteUser
            RemoteMachine     = $AssociatedNetFirewallRule.SecurityRemoteMachine
        }
    }
}

function Disable-NetFirewallRule {
    param([Parameter(ValueFromPipeline = $true)]$InputObject)
    process {
        $Script:MockCallLog.Add("Disable:$($InputObject.Name)")
        $InputObject.Enabled = $false
    }
}

function Set-NetFirewallRule {
    param([Parameter(ValueFromPipeline = $true)]$InputObject, [string]$Profile, $Enabled, [string]$Action)
    process {
        $Script:MockCallLog.Add("Set:$($InputObject.Name):Profile=$($Profile):Enabled=$($Enabled):Action=$($Action)")
        if ($PSBoundParameters.ContainsKey('Profile')) {
            $sum = 0
            foreach ($name in $Profile -split ',') { $sum += $Script:OrangeProfileBits[$name] }
            $InputObject.Profile = $sum
        }
        if ($PSBoundParameters.ContainsKey('Enabled')) { $InputObject.Enabled = $Enabled }
        if ($PSBoundParameters.ContainsKey('Action')) { $InputObject.Action = $Action }
    }
}

function New-NetFirewallRule {
    param([string]$Name, [string]$DisplayName, [string]$Direction, [string]$Action, [string]$Protocol, [string]$Program, [string]$Profile, [string]$PolicyStore, $Enabled)
    $Script:MockCallLog.Add("New:$($Name):$($Protocol):$($Profile):Store=$($PolicyStore)")
    New-OrangeMockCimRule -Name $Name -Program $Program -Direction $Direction -Action $Action -Protocol $Protocol `
        -Profile (($Profile -split ',' | ForEach-Object { $Script:OrangeProfileBits[$_] } | Measure-Object -Sum).Sum) -Enabled $Enabled | Out-Null
}

function Get-Process {
    param([int]$Id, [string]$ErrorAction)
    $Script:MockGetProcessCallCount += 1
    if ($Script:MockOnGetProcess) { & $Script:MockOnGetProcess $Id $Script:MockGetProcessCallCount }
    if ($Script:MockAliveRequesterPid -and [int]$Script:MockAliveRequesterPid -eq $Id) {
        return [pscustomobject]@{ Id = $Id }
    }
    return $null
}

function Assert-True {
    param([bool]$Condition, [string]$Message)
    if (-not $Condition) { throw "assertion failed: $Message" }
}

function Assert-ThrowsReason {
    param([scriptblock]$Body, [string]$Reason)
    $caught = $null
    try { & $Body } catch { $caught = $_.Exception.Message }
    Assert-True ([string]::IsNullOrEmpty($caught) -eq $false) "expected throw '$Reason'"
    Assert-True ($caught -eq $Reason) "expected reason '$Reason', got '$caught'"
}

function Reset-OrangeMockState {
    $Script:MockCimAppFilters = @{}
    $Script:MockCimRulesByFilter = @{}
    $Script:MockCimRulesByName = @{}
    $Script:MockCallLog = New-Object System.Collections.Generic.List[string]
    $Script:MockAliveRequesterPid = $PID
    $Script:MockGetProcessCallCount = 0
    $Script:MockOnGetProcess = $null
}

function Test-RuleWithoutCimProtocolPropertyDoesNotCrash {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy -Rules @((New-OrangeMockComRule -ApplicationName $exe -Protocol 17))
    $facts = Get-OrangeFacts -Fw $fw -Exe $exe
    Assert-True ($facts.appRules.Count -eq 1) 'expected exactly one matched app rule'
    Assert-True ($facts.appRules[0].protocol -eq 'UDP') 'expected protocol UDP decoded from COM'
}

function Test-NoActiveProfileFailsClosedRatherThanAssumingPublic {
    Reset-OrangeMockState
    $fw = New-OrangeMockFwPolicy -CurrentProfileTypes 0
    Assert-ThrowsReason { Get-OrangeActiveProfiles -Fw $fw | Out-Null } 'no-active-profile'
}

function Test-UnknownActiveProfileBitsFailClosed {
    Reset-OrangeMockState
    $fw = New-OrangeMockFwPolicy -CurrentProfileTypes 8
    Assert-ThrowsReason { Get-OrangeActiveProfiles -Fw $fw | Out-Null } 'unrecognized-active-profile'
}

function Test-RestrictionClassifierFailsClosedOnPropertyReadError {
    Reset-OrangeMockState
    $rule = New-OrangeMockComRule -ApplicationName 'C:\apps\orange.exe' -ThrowOnRestrictionRead
    Assert-True (Test-OrangeRuleRestricted -Rule $rule) 'property read failure must count as restricted'
}

function Test-ComInterfacesRestrictionIsNotWideOpen {
    Reset-OrangeMockState
    $rule = New-OrangeMockComRule -ApplicationName 'C:\apps\orange.exe' -Interfaces @('Ethernet')
    Assert-True (Test-OrangeRuleRestricted -Rule $rule) 'named interface list must be restricted'
}

function Test-ScanLimitThrowsOn33rdMatchingOwnRule {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $rules = @()
    for ($i = 0; $i -lt 32; $i++) {
        $rules += New-OrangeMockComRule -ApplicationName $exe -Action 1
    }
    $rules += New-OrangeMockComRule -ApplicationName $exe -Action 0
    $fw = New-OrangeMockFwPolicy -Rules $rules
    Assert-ThrowsReason { Get-OrangeFacts -Fw $fw -Exe $exe | Out-Null } 'scan-limit'
}

function Test-ManagedOriginIncludesComLocalPolicyGuard {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy -LocalPolicyModifyState 1
    $facts = Get-OrangeFacts -Fw $fw -Exe $exe
    Assert-True $facts.managedOrigin 'non-zero LocalPolicyModifyState must set managedOrigin'
}

function Test-ManagedOriginIsDetectedForMatchingInboundBlockRule {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'gpo-rule' -Program $exe -PolicyStoreSourceType 'Gpo')
    $managed = Test-OrangeAnyManagedOrigin -Exe $exe -ActiveProfiles @('Public')
    Assert-True $managed 'gpo block rule must be managed'
}

function Test-CimEligibilityRejectsMismatchedApplicationFilterProgram {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $rule = New-OrangeMockCimRule -Name 'local-block' -Program $exe -ApplicationFilters @([pscustomobject]@{ Program = 'C:\apps\other.exe'; Package = 'Any' })
    Assert-True (-not (Test-OrangeCimRuleEligibleForDisable -Rule $rule -Exe $exe)) 'mismatched app filter Program must fail'
}

function Test-CimEligibilityRejectsMultipleApplicationFilters {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $filters = @([pscustomobject]@{ Program = $exe; Package = 'Any' }, [pscustomobject]@{ Program = $exe; Package = 'Any' })
    $rule = New-OrangeMockCimRule -Name 'local-block' -Program $exe -ApplicationFilters $filters
    Assert-True (-not (Test-OrangeCimRuleEligibleForDisable -Rule $rule -Exe $exe)) 'multiple app filters must fail closed'
}

function Test-CimEligibilityRejectsInterfaceAliasRestriction {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $rule = New-OrangeMockCimRule -Name 'local-block' -Program $exe -InterfaceAlias @('Ethernet')
    Assert-True (-not (Test-OrangeCimRuleEligibleForDisable -Rule $rule -Exe $exe)) 'named interface alias must fail eligibility'
}

function Test-CimEligibilityRejectsSecurityRestriction {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $rule = New-OrangeMockCimRule -Name 'local-block' -Program $exe -SecurityAuthentication 'Required'
    Assert-True (-not (Test-OrangeCimRuleEligibleForDisable -Rule $rule -Exe $exe)) 'security auth requirement must fail eligibility'
}

function Test-RepairDisablesOnlyTheUnrestrictedLocalWholeAppBlockRule {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'local-block-tcp' -Program $exe -Protocol 'TCP' -Profile 4)
    $cancelled = Repair-OrangeAppBlocks -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID
    Assert-True (-not $cancelled) 'repair must not cancel when requester alive'
    Assert-True ($Script:MockCallLog -contains 'Disable:local-block-tcp') 'expected disable of unrestricted block'
}

function Test-RepairNarrowsRatherThanDisablesARuleCoveringAnInactiveProfileToo {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'multi-profile-block' -Program $exe -Profile 5)
    Repair-OrangeAppBlocks -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null
    Assert-True (-not ($Script:MockCallLog -contains 'Disable:multi-profile-block')) 'must not disable rule still covering inactive profile'
    $narrowed = $Script:MockCallLog | Where-Object { $_ -like 'Set:multi-profile-block:Profile=Domain*' }
    Assert-True ([bool]$narrowed) 'expected narrowing to Domain'
}

function Test-RepairSkipsARestrictedLocalBlockRule {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'restricted-block' -Program $exe -LocalPort '3478')
    Repair-OrangeAppBlocks -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null
    Assert-True ($Script:MockCallLog.Count -eq 0) 'restricted block must not be mutated'
}

function Test-RepairAbortsWhenRequesterIsNotAlive {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'local-block' -Program $exe)
    $Script:MockAliveRequesterPid = $null
    $cancelled = Repair-OrangeAppBlocks -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid '99999999'
    Assert-True $cancelled 'expected cancellation when requester gone'
    Assert-True ($Script:MockCallLog.Count -eq 0) 'must not mutate after cancellation'
}

function Test-AddAllowCreatesBothTransportsWhenNoneExistInPersistentStore {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Add-OrangeAppAllow -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null
    Assert-True ((@($Script:MockCallLog | Where-Object { $_ -like 'New:*:TCP:*Store=PersistentStore' })).Count -eq 1) 'expected one TCP persistent-store allow creation'
    Assert-True ((@($Script:MockCallLog | Where-Object { $_ -like 'New:*:UDP:*Store=PersistentStore' })).Count -eq 1) 'expected one UDP persistent-store allow creation'
}

function Test-AddAllowThrowsOnNameCollisionWithDifferentProgram {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $name = New-OrangeRuleName -Exe $exe -Transport 'TCP'
    New-OrangeMockCimRule -Name $name -Program 'C:\apps\different.exe' -Action 'Allow' -Protocol 'TCP' | Out-Null
    $fw = New-OrangeMockFwPolicy
    Assert-ThrowsReason { Add-OrangeAppAllow -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null } 'allow-rule-collision'
}

function Test-AddAllowThrowsOnNamedRuleWrongDirectionOrProtocol {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $tcpName = New-OrangeRuleName -Exe $exe -Transport 'TCP'
    $udpName = New-OrangeRuleName -Exe $exe -Transport 'UDP'
    New-OrangeMockCimRule -Name $tcpName -Program $exe -Action 'Allow' -Direction 'Outbound' -Protocol 'TCP' | Out-Null
    New-OrangeMockCimRule -Name $udpName -Program $exe -Action 'Allow' -Direction 'Inbound' -Protocol 'UDP' | Out-Null
    $fw = New-OrangeMockFwPolicy
    Assert-ThrowsReason { Add-OrangeAppAllow -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null } 'allow-rule-collision'
}

function Test-AddAllowReactivatingADisabledRuleDoesNotResurrectInactiveProfiles {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    $name = New-OrangeRuleName -Exe $exe -Transport 'TCP'
    $rule = New-OrangeMockCimRule -Name $name -Program $exe -Action 'Allow' -Enabled $false -Profile 3 -Protocol 'TCP'
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule $rule
    Add-OrangeAppAllow -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null
    $setCall = $Script:MockCallLog | Where-Object { $_ -like "Set:$name*" } | Select-Object -First 1
    Assert-True ([bool]$setCall) 'expected disabled rule reconciliation'
    Assert-True ($setCall -like '*Profile=Public*') "expected only active profile, got: $setCall"
}

function Test-RepairPreflightsAllowTargetsBeforeDisablingBlocks {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'local-block' -Program $exe)
    $tcpName = New-OrangeRuleName -Exe $exe -Transport 'TCP'
    New-OrangeMockCimRule -Name $tcpName -Program $exe -Action 'Allow' -Direction 'Outbound' -Protocol 'TCP' | Out-Null
    Assert-ThrowsReason { Invoke-OrangeRepair -Fw $fw -Exe $exe -RequesterPid $PID | Out-Null } 'allow-rule-collision'
    Assert-True ((@($Script:MockCallLog | Where-Object { $_ -like 'Disable:*' })).Count -eq 0) 'block disable must not happen after failed preflight'
}

function Test-RepairFailsWhenLocalPolicyModifyStateDisallowsChanges {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy -LocalPolicyModifyState 1
    Assert-ThrowsReason { Invoke-OrangeRepair -Fw $fw -Exe $exe -RequesterPid $PID | Out-Null } 'policy-changed'
}

function Test-RepairFailsWhenActiveProfilesChangeBeforeMutation {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy -CurrentProfileTypes 4
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'local-block' -Program $exe)
    $Script:MockOnGetProcess = {
        param($id, $count)
        if ($count -eq 2) { $fw.CurrentProfileTypes = 2 }
    }.GetNewClosure()
    Assert-ThrowsReason { Invoke-OrangeRepair -Fw $fw -Exe $exe -RequesterPid $PID | Out-Null } 'profile-changed'
    Assert-True ((@($Script:MockCallLog | Where-Object { $_ -like 'Disable:*' })).Count -eq 0) 'must not mutate once profile drifts'
}

function Test-RepairDoesNotWriteToAnEntirelyInactiveRule {
    Reset-OrangeMockState
    $exe = 'C:\apps\orange.exe'
    $fw = New-OrangeMockFwPolicy
    Register-OrangeMockCimRuleForExe -Exe $exe -Rule (New-OrangeMockCimRule -Name 'inactive-domain-block' -Program $exe -Profile 1)
    Repair-OrangeAppBlocks -Fw $fw -Exe $exe -ActiveProfiles @('Public') -RequesterPid $PID | Out-Null
    Assert-True ($Script:MockCallLog.Count -eq 0) 'an entirely inactive rule must not receive even a no-op write'
}

$scenarios = @(
    'Test-RepairDoesNotWriteToAnEntirelyInactiveRule',
    'Test-RuleWithoutCimProtocolPropertyDoesNotCrash',
    'Test-NoActiveProfileFailsClosedRatherThanAssumingPublic',
    'Test-UnknownActiveProfileBitsFailClosed',
    'Test-RestrictionClassifierFailsClosedOnPropertyReadError',
    'Test-ComInterfacesRestrictionIsNotWideOpen',
    'Test-ScanLimitThrowsOn33rdMatchingOwnRule',
    'Test-ManagedOriginIncludesComLocalPolicyGuard',
    'Test-ManagedOriginIsDetectedForMatchingInboundBlockRule',
    'Test-CimEligibilityRejectsMismatchedApplicationFilterProgram',
    'Test-CimEligibilityRejectsMultipleApplicationFilters',
    'Test-CimEligibilityRejectsInterfaceAliasRestriction',
    'Test-CimEligibilityRejectsSecurityRestriction',
    'Test-RepairDisablesOnlyTheUnrestrictedLocalWholeAppBlockRule',
    'Test-RepairNarrowsRatherThanDisablesARuleCoveringAnInactiveProfileToo',
    'Test-RepairSkipsARestrictedLocalBlockRule',
    'Test-RepairAbortsWhenRequesterIsNotAlive',
    'Test-AddAllowCreatesBothTransportsWhenNoneExistInPersistentStore',
    'Test-AddAllowThrowsOnNameCollisionWithDifferentProgram',
    'Test-AddAllowThrowsOnNamedRuleWrongDirectionOrProtocol',
    'Test-AddAllowReactivatingADisabledRuleDoesNotResurrectInactiveProfiles',
    'Test-RepairPreflightsAllowTargetsBeforeDisablingBlocks',
    'Test-RepairFailsWhenLocalPolicyModifyStateDisallowsChanges',
    'Test-RepairFailsWhenActiveProfilesChangeBeforeMutation'
)

$failures = 0
foreach ($scenario in $scenarios) {
    try {
        & $scenario
        Write-Output "PASS $scenario"
    } catch {
        Write-Output "FAIL $scenario : $_"
        $failures += 1
    }
}

if ($failures -gt 0) {
    Write-Output "$failures scenario(s) failed"
    exit 1
}

Write-Output 'all scenarios passed'
exit 0
