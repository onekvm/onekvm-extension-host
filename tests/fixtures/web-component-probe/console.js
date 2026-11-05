window.OneKVMPluginUI.v1.console.register('web-component-probe', {
  apiVersion: 1,
  onLoad(host) {
    const probe = window.__onekvmConsoleProbe ||= { loads: [], cleanups: [], aborts: [] }
    const version = host.extension.version
    probe.loads.push(version)
    probe.host = host
    const node = document.createElement('aside')
    node.id = 'onekvm-console-probe'
    node.dataset.toolboxLocal = ''
    node.textContent = `Console hook ${version}`
    node.style.cssText = 'position:fixed;bottom:12px;right:12px;z-index:1000;padding:8px;background:var(--card);color:var(--foreground);border:1px solid var(--border)'
    document.body.append(node)
    host.signal.addEventListener('abort', () => probe.aborts.push(version), { once: true })
    return () => { node.remove(); probe.cleanups.push(version) }
  },
})
