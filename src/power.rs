/// Suspend the system. Best-effort: failures are logged, never propagated.
/// Args to `SetSuspendState`: hibernate=false (sleep, not hibernate),
/// force=false (let apps veto), wake_event_disabled=false (keyboard/power
/// button can still wake).
pub fn suspend_to_sleep() {
    use windows::Win32::System::Power::SetSuspendState;
    let ok = unsafe { SetSuspendState(false, false, false) };
    if !ok {
        tracing::warn!("SetSuspendState returned false — system did not enter sleep");
    } else {
        tracing::info!("system suspended");
    }
}
