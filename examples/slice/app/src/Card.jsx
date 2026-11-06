// The "Card" frame (node 1:1) of examples/slice/figma/card-file.json, built
// from examples/slice/spec/card.spec.json (`figmog spec 1:1`).
//
// Every value the spec binds to a token uses that token: a Tailwind utility
// generated from the exported @theme (bg-surface, rounded-md, py-4, gap-4,
// gap-2, bg-brand, text-text-primary) or, for tokens the export emits
// unitless and without a theme entry (font sizes, weight, line height,
// stroke width), the CSS variable itself. Values the spec flags
// `untokenized` have no token to use and are written as arbitrary values,
// each marked "untokenized" below.
export function Card() {
  return (
    <article
      data-testid="slice-card"
      className={[
        "flex flex-col items-start",
        "w-[320px]", // untokenized: width 320 (FIXED sizing)
        "gap-4 py-4", // space/4
        "px-[24px]", // untokenized: padding left/right 24
        "rounded-md", // radius/md
        "bg-surface", // surface
        // border/subtle + stroke/thin; strokeAlign INSIDE, so an inset box-shadow (no layout effect)
        "ring-inset ring-border-subtle ring-[length:calc(var(--stroke-thin)*1px)]",
        "font-[Inter,sans-serif]", // font/family/sans is a STRING token: not exported
      ].join(" ")}
    >
      <h2
        className={[
          "w-full",
          "text-text-primary", // text/primary
          "text-[length:calc(var(--font-size-title)*1px)]", // font/size/title
          "font-[number:var(--font-weight-bold)]", // font/weight/bold
          "leading-[28px]", // untokenized: line height 28
          "tracking-[0px]", // untokenized: letter spacing 0
        ].join(" ")}
      >
        Spring garden plan
      </h2>
      <div className="flex w-full items-center justify-center gap-2">
        <p
          className={[
            "grow",
            "text-[#1a1a1f]", // untokenized fill (spec lists matches: ink/900, text/primary)
            "text-[length:calc(var(--font-size-body)*1px)]", // font/size/body
            "leading-[calc(var(--font-line-body)*1px)]", // font/line/body
            "font-[number:400]", // untokenized: weight 400
            "tracking-[0px]", // untokenized: letter spacing 0
          ].join(" ")}
        >
          Four raised beds, sown in May
        </p>
        <span
          aria-hidden="true"
          className={[
            "shrink-0",
            "h-[16px] w-[24px]", // untokenized: 24 x 16 (FIXED sizing)
            "rounded-[4px]", // untokenized: radius 4
            "bg-brand", // brand
          ].join(" ")}
        />
      </div>
    </article>
  );
}
