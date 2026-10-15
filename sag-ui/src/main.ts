import { createPinia } from "pinia"
import { createApp } from "vue"
import { installI18n } from "@/lib/i18n"
import App from "./App.vue"
import router from "./router"
import "./style.css"

const app = createApp(App)

app.use(createPinia())
app.use(router)
installI18n(app)

app.mount("#app")
