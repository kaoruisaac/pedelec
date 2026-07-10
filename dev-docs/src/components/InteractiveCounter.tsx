import { createSignal } from 'solid-js';

type Props = {
  decrementLabel: string;
  incrementLabel: string;
  statusLabel: string;
};

export default function InteractiveCounter(props: Props) {
  const [count, setCount] = createSignal(0);

  return (
    <section class="counter-demo" aria-live="polite">
      <p>{props.statusLabel}: {count()}</p>
      <div class="counter-actions">
        <button type="button" onClick={() => setCount((value) => value - 1)}>
          {props.decrementLabel}
        </button>
        <button type="button" onClick={() => setCount((value) => value + 1)}>
          {props.incrementLabel}
        </button>
      </div>
    </section>
  );
}
