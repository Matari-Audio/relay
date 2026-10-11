The editor retains MUI graphics reporting until native resources are released. Cargo.lock pins the application and MOOSE to one MUI source; the plugin UI build checks that the report revision matches every MUI package. MUI_REPORTING_DISABLED=1 disables delivery; MUI_RENDERER=cpu forces software presentation. Failed delivery acknowledgements retain the bounded local queue for retry.

Public pull requests and main pushes build a Linux VST3 on standard free runners, then run the shared MUI editor validator with software Vulkan, GL and forced CPU. It requires an actual editor-open record and the corresponding MUI presentation breadcrumb, and retains pluginval logs, native screen recording and graphics/MUI diagnostics for seven days, including after failure.

The reporting service is deployed; automatic public MUI issues currently await the service token's Issues read/write access. Tests do not send customer reports. Real DAWs and NVIDIA/Intel hardware remain unverified.

Settled editors stop rebuilding every frame. Meter tails, delayed text commits,
link metadata, parameter/input changes and connected-state animation still wake
the UI. Open microphone panels keep polling for resize layout convergence;
this is documented until Moose offers an after-frame callback. The hosted UI
workflow runs the scheduling regressions before building the VST3. No measured
performance improvement is claimed before native profiling.
