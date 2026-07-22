import {
    For,
    Setter,
    Show,
    createContext,
    createEffect,
    createSignal,
    createUniqueId,
    onCleanup,
    onMount,
    useContext,
    type JSX
} from "solid-js";
import './PopUpProvider.css';

export type PopUpOptions = {
    background?: boolean;
    closeOnBackground?: boolean;
}

type PopUpPosition = { x: number; y: number };

export type PopFunction = <P>(
    component: (props: P) => JSX.Element,
    componentProps: P,
    options?: PopUpOptions
  ) => PopUp<P>;
const clamp = (value: number, min: number, max: number) => Math.min(Math.max(value, min), max);

const PopUpContext = createContext<{
    pop: PopFunction;
}>();

export class PopUp<P> {
    id: string;
    content: (props: P) => JSX.Element;
    componentProps: P;
    onPopUpRemove: () => void;
    setActive: Setter<boolean> | undefined;
    setDraggableElement: ((element: HTMLElement) => void) = () => {};
    options: PopUpOptions;

    constructor(content: (props: P) => JSX.Element, componentProps: P, onPopUpRemove: () => void, options: PopUpOptions) {
        this.id = createUniqueId();
        this.content = content;
        this.componentProps = componentProps;
        this.onPopUpRemove = onPopUpRemove;
        this.options = {
            background: true,
            closeOnBackground: true,
        };
        Object.assign(this.options, options);
    }

    close() {
        if (this.setActive) {
            this.setActive(false);
        }
        setTimeout(() => this.onPopUpRemove(), 300);
    }

    render() {
        const [active, setActive] = createSignal(false);
        const [draggablePosition, setDraggablePosition] = createSignal<PopUpPosition | null>(null);
        const [isDragging, setIsDragging] = createSignal(false);
        const [draggableHandle, setDraggableHandle] = createSignal<HTMLElement | null>(null);
        let popupContentElement: HTMLElement;
        this.setActive = setActive;
        this.setDraggableElement = setDraggableHandle;

        onMount(() => {
            setTimeout(() => setActive(true), 0);
        });

        createEffect(() => {
            const handle = draggableHandle();
            if (!handle) {
                return;
            }

            let activePointerId: number | null = null;
            let dragOffset = { x: 0, y: 0 };

            const onPointerDown = (event: PointerEvent) => {
                if (event.button !== 0) {
                    return;
                }
                if ((event.target as HTMLElement).closest('button')) {
                    return;
                }
                if (!popupContentElement?.parentElement) {
                    return;
                }

                const contentBounds = popupContentElement.getBoundingClientRect();
                const frameBounds = popupContentElement.parentElement.getBoundingClientRect();

                dragOffset = {
                    x: event.clientX - contentBounds.left,
                    y: event.clientY - contentBounds.top,
                };

                setDraggablePosition({
                    x: contentBounds.left - frameBounds.left,
                    y: contentBounds.top - frameBounds.top,
                });
                setIsDragging(true);

                activePointerId = event.pointerId;
                handle.setPointerCapture(event.pointerId);
                event.preventDefault();
                event.stopPropagation();
            };

            const onPointerMove = (event: PointerEvent) => {
                if (activePointerId !== event.pointerId || !popupContentElement?.parentElement) {
                    return;
                }

                const contentBounds = popupContentElement.getBoundingClientRect();
                const frameBounds = popupContentElement.parentElement.getBoundingClientRect();

                const x = event.clientX - dragOffset.x - frameBounds.left;
                const y = event.clientY - dragOffset.y - frameBounds.top;
                const maxX = Math.max(0, frameBounds.width - contentBounds.width);
                const maxY = Math.max(0, frameBounds.height - contentBounds.height);

                setDraggablePosition({
                    x: clamp(x, 0, maxX),
                    y: clamp(y, 0, maxY),
                });

                event.preventDefault();
                event.stopPropagation();
            };

            const finishDrag = (event: PointerEvent) => {
                if (activePointerId !== event.pointerId) {
                    return;
                }

                activePointerId = null;
                setIsDragging(false);
                handle.releasePointerCapture(event.pointerId);
                event.preventDefault();
                event.stopPropagation();
            };

            handle.addEventListener('pointerdown', onPointerDown);
            handle.addEventListener('pointermove', onPointerMove);
            handle.addEventListener('pointerup', finishDrag);
            handle.addEventListener('pointercancel', finishDrag);

            onCleanup(() => {
                handle.removeEventListener('pointerdown', onPointerDown);
                handle.removeEventListener('pointermove', onPointerMove);
                handle.removeEventListener('pointerup', finishDrag);
                handle.removeEventListener('pointercancel', finishDrag);
            });
        });

        return (
            <div class="PopUpFrame" classList={{ active: active() }}>
                <Show when={this.options.background}>
                    <div class="PopUpBackground" onClick={() => this.options.closeOnBackground !== false && this.close()} />
                </Show>
                <div
                    class="PopUpContent"
                    classList={{ 'is-dragging': isDragging() }}
                    ref={el => popupContentElement = el}
                    style={draggablePosition() ? { position: 'absolute', top: `${draggablePosition()!.y}px`, left: `${draggablePosition()!.x}px` } : {}}>
                    {this.content({ ...this.componentProps, popup: this })}
                </div>
            </div>
        )
    }
}

const PopUpProvider = (props: { children: JSX.Element }) => {
    const [popUps, setPopUps] = createSignal<PopUp<unknown>[]>([]);

    function createPopUp<P>(component: (props: P) => JSX.Element, componentProps: P, options: PopUpOptions = {} as PopUpOptions): PopUp<P> {
        const newPopUp = new PopUp(component, componentProps, () => setPopUps((prev: PopUp<unknown>[]) => prev.filter((p) => p.id !== newPopUp.id)), options);
        setPopUps((prev: PopUp<unknown>[]) => [...prev, newPopUp as PopUp<unknown>]);
        return newPopUp;
    }

    const pop = createPopUp;

    const value = {
        pop,
    }

    return (
        <PopUpContext.Provider value={value}>
            {props.children}
            <div class="PopUpProvider">
                <For each={popUps()}>
                    {(popUp) => popUp.render()}
                </For>
            </div>
        </PopUpContext.Provider>
    )
}

export default PopUpProvider;

export const usePopUp = () => {
    const context = useContext(PopUpContext);
    if (!context) {
        throw new Error('usePopUp must be used within PopUpProvider');
    }
    return context;
}

export function forwardPopUp<P>(wrapper: (popup: PopUp<P>, props: P) => JSX.Element) {
    return (props: P) => wrapper((props as unknown as { popup: PopUp<P> }).popup as PopUp<P>, props)
}
