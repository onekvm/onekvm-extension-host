(() => {
  const runtime = window.OneKVMPluginUI.v1
  const copies = {
    default: {title: 'Native JavaScript', message: 'Message', invoke: 'Call extension', increment: 'Increment', resize: 'Resize window', count: 'Count'},
    zh: {title: '原生 JavaScript', message: '消息', invoke: '调用扩展', increment: '加一', resize: '调整窗口', count: '计数'},
    'zh-TW': {title: '原生 JavaScript', message: '訊息', invoke: '呼叫擴充', increment: '加一', resize: '調整視窗', count: '計數'},
  }
  class NativeView extends HTMLElement {
    constructor() {
      super()
      this.count = 0
      this.attachShadow({mode: 'open'}).innerHTML = `<style>
        :host{display:block;color:var(--foreground);font:14px/1.5 system-ui}
        section{display:grid;gap:12px}h3,p{margin:0}label{display:grid;gap:4px}
        input,button{font:inherit;border:1px solid var(--border);border-radius:6px;padding:7px 10px;color:var(--foreground);background:var(--card)}
        button{cursor:pointer}button:focus-visible,input:focus-visible{outline:2px solid var(--ring)}
        nav{display:flex;flex-wrap:wrap;gap:8px}output{overflow-wrap:anywhere}
      </style><section><h3></h3><p id="identity"></p><label><span></span><input maxlength="120"></label><nav><button id="increment"></button><button id="invoke"></button><button id="resize"></button></nav><p id="count"></p><output role="status"></output></section>`
    }
    connectedCallback() {
      if (!this.host) throw Error('Host must be assigned before connection')
      this.abort = new AbortController()
      const input = this.shadowRoot.querySelector('input')
      input.value = this.host.extension.settings.message
      this.settings = this.host.registerSettings({
        collect: () => ({message: input.value}),
        reset: settings => { input.value = settings.message; this.settings.update({dirty: false}) },
      })
      input.addEventListener('input', () => this.settings.update({dirty: input.value !== this.host.extension.settings.message}), {signal: this.abort.signal})
      this.addEventListener('onekvm-host-change', () => this.render(), {signal: this.abort.signal})
      this.shadowRoot.getElementById('increment').addEventListener('click', () => { this.count++; this.render() }, {signal: this.abort.signal})
      this.shadowRoot.getElementById('invoke').addEventListener('click', async () => {
        try { this.shadowRoot.querySelector('output').textContent = JSON.stringify(await this.host.invoke('echo')) }
        catch (error) { this.shadowRoot.querySelector('output').textContent = error.message }
      }, {signal: this.abort.signal})
      this.shadowRoot.getElementById('resize').addEventListener('click', () => this.host.resizeWindow?.({width: 500, height: 420}), {signal: this.abort.signal})
      this.render()
    }
    render() {
      const text = copies[this.host.language === 'zh_tw' ? 'zh-TW' : this.host.language] || copies.default
      this.shadowRoot.querySelector('h3').textContent = text.title
      this.shadowRoot.getElementById('identity').textContent = this.host.extension.id + ' · ' + this.host.extension.version
      this.shadowRoot.querySelector('label span').textContent = text.message
      for (const id of ['increment', 'invoke', 'resize']) this.shadowRoot.getElementById(id).textContent = text[id]
      this.shadowRoot.getElementById('resize').hidden = !this.host.resizeWindow
      this.shadowRoot.getElementById('count').textContent = text.count + ': ' + this.count
    }
    disconnectedCallback() { this.abort?.abort(); this.settings?.dispose() }
  }
  const definition = {apiVersion: 1, element: NativeView}
  if (new URL(document.currentScript.src).pathname.endsWith('/native-tool.js')) runtime.toolbox.register('web-component-probe', 'native', definition)
  else runtime.register('web-component-probe', definition)
})()
