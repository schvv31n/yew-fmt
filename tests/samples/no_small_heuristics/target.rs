// config: yew.use_small_heuristics="Off",yew.html_flavor="Ext"

use yew::prelude::*;

#[function_component]
fn Comp() -> Html {
    html! {
        <>
            <div>
                <code>
                    { "Код!" }
                </code>
            </div>
            if true {
                { "true" }
            } else {
                { "false" }
            }
            for i in 0 .. 10 {
                { i }
            }
        </>
    }
}
