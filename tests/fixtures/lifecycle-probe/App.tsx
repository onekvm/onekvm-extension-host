import { View, Text } from '@pocketjs/framework/vue-vapor/components'
import { onFrame } from '@pocketjs/framework/vue-vapor/lifecycle'

export default function Probe() {
  const bridge = (globalThis as any).onekvm
  onFrame(() => {
    if (bridge?.get('knobClick') === '1') bridge.set('launch', 'launcher')
  })
  return <View class="w-full h-full flex flex-col items-center justify-center bg-black">
    <Text class="text-base text-white">麒麟生命周期测试 麒麟生命週期測試 Lifecycle probe</Text>
    <View focusable onPress={() => bridge?.set('launch', 'launcher')}><Text class="text-base text-white">Back 返回</Text></View>
  </View>
}
