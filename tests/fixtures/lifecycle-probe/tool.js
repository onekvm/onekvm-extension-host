(() => {
  const { vue, toolbox } = window.OneKVMPluginUI.v1
  toolbox.register('lifecycle-probe', 'probe', {
    apiVersion: 1,
    component: vue.defineComponent({
      setup: () => () => vue.h('p', 'Lifecycle tool loaded'),
    }),
  })
})()
