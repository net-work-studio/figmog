// HARDCODED VARIANT (test fixture, never shipped): looks exactly like
// src/Card.jsx with today's token values, but writes every value as a
// literal instead of using the tokens. The token-perturbation test must FAIL
// for this card: changing a token does not change it.
//
// The "Card" frame (node 1:1) of examples/slice/figma/card-file.json, built
// from examples/slice/spec/card.spec.json (`figmog spec 1:1`).
//
// Same layout as src/Card.jsx; the values the spec binds to tokens are
// literals here (marked "hardcoded").
export function Card() {
  return (
    <article
      data-testid="slice-card"
      className={[
        "flex flex-col items-start",
        "w-[320px]", // untokenized: width 320 (FIXED sizing)
        "gap-[16px] py-[16px]", // hardcoded space/4
        "px-[24px]", // untokenized: padding left/right 24
        "rounded-[12px]", // hardcoded radius/md
        "bg-[#fafafc]", // hardcoded surface
        // border/subtle + stroke/thin; strokeAlign INSIDE, so an inset box-shadow (no layout effect)
        "ring-inset ring-[#e0e3e6] ring-[length:1px]", // hardcoded
        "font-[Inter,sans-serif]", // font/family/sans is a STRING token: not exported
      ].join(" ")}
    >
      <h2
        className={[
          "w-full",
          "text-[#1a1a1f]", // hardcoded text/primary
          "text-[length:20px]", // hardcoded font/size/title
          "font-[number:700]", // hardcoded font/weight/bold
          "leading-[28px]", // untokenized: line height 28
          "tracking-[0px]", // untokenized: letter spacing 0
        ].join(" ")}
      >
        Spring garden plan
      </h2>
      <div className="flex w-full items-center justify-center gap-[8px]">
        <p
          className={[
            "grow",
            "text-[#1a1a1f]", // untokenized fill (spec lists matches: ink/900, text/primary)
            "text-[length:16px]", // hardcoded font/size/body
            "leading-[24px]", // hardcoded font/line/body
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
            "bg-[#0066cc]", // hardcoded brand
          ].join(" ")}
        />
      </div>
    </article>
  );
}
