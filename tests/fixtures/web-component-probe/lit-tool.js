import { LitElement, html, css } from 'lit'

class LitTool extends LitElement {
  static properties = { host: {attribute: false}, count: {state: true}, result: {state: true} }
  static styles = css`
    :host{display:block;color:var(--foreground);font:14px/1.5 system-ui}
    section{display:grid;gap:12px}h3,p{margin:0}nav{display:flex;gap:8px}
    button{font:inherit;border:1px solid var(--border);border-radius:6px;padding:7px 10px;color:var(--foreground);background:var(--card);cursor:pointer}
    button:focus-visible{outline:2px solid var(--ring)}output{overflow-wrap:anywhere}
  `
  constructor() { super(); this.count = 0; this.result = ''; this.changed = () => this.requestUpdate() }
  connectedCallback() { super.connectedCallback(); if(!this.host) throw Error('Missing host'); this.addEventListener('onekvm-host-change', this.changed) }
  disconnectedCallback() { this.removeEventListener('onekvm-host-change', this.changed); super.disconnectedCallback() }
  async invoke() { try { this.result = JSON.stringify(await this.host.invoke('echo')) } catch(error) { this.result = error.message } }
  render() {
    const language = this.host?.language
    const text = language === 'zh' ? ['加一','调用扩展','调整窗口','计数'] : language === 'zh_tw' ? ['加一','呼叫擴充','調整視窗','計數'] : ['Increment','Call extension','Resize window','Count']
    return html`<section><h3>Lit</h3><p>${this.host?.extension.id} · ${this.host?.extension.version}</p>
      <nav><button id="increment" @click=${() => this.count++}>${text[0]}</button><button id="invoke" @click=${() => this.invoke()}>${text[1]}</button><button id="resize" @click=${() => this.host.resizeWindow({width: 500, height: 420})}>${text[2]}</button></nav>
      <p id="count">${text[3]}: ${this.count}</p><output role="status">${this.result}</output></section>`
  }
}
window.OneKVMPluginUI.v1.toolbox.register('web-component-probe', 'lit', {apiVersion: 1, element: LitTool})
