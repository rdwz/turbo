import Image from "next/image";
import Link from "next/link";
import { HeroText, SectionSubtext } from "../home-shared/Headings";
import { Gradient } from "../home-shared/Gradient";
import { FadeIn } from "../home-shared/FadeIn";
import { CTAButton } from "../home-shared/CTAButton";
import { PackLogo } from "../../logos/PackLogo";

export function PackHero() {
  return (
    <FadeIn
      className="font-sans w-auto min-h-[calc(100svh-var(--nextra-navbar-height))] pb-16 pt-[48px] md:pb-24 lg:pb-32 md:pt-16 lg:pt-20 flex justify-between gap-8 items-center flex-col relative z-0"
      noVertical
    >
      <FadeIn className="z-50 flex items-center justify-center w-full">
        <div className="absolute min-w-[614px] min-h-[614px]">
          <Image
            alt="Turbopack"
            className="hidden dark:block"
            height={614}
            src="/images/docs/pack/turbopack-hero-hexagons-dark.svg"
            width={614}
          />
          <Image
            alt="Turbopack"
            className="block dark:hidden"
            height={614}
            src="/images/docs/pack/turbopack-hero-hexagons-light.svg"
            width={614}
          />
        </div>
        <div className="absolute z-50 flex items-center justify-center w-64 h-64">
          <Gradient
            className="dark:opacity-100 opacity-40"
            conic
            height={120}
            small
            width={120}
          />
        </div>

        <div className="w-[120px] z-50 mt-[-8.075px] mb-[-8.075px]">
          <Image
            alt=""
            className="hidden dark:block"
            height={136.15}
            src="/images/docs/pack/turbopack-hero-logo-dark.svg"
            width={120}
          />
          <Image
            alt=""
            className="block dark:hidden"
            height={136.15}
            src="/images/docs/pack/turbopack-hero-logo-light.svg"
            width={120}
          />
        </div>
      </FadeIn>
      <Gradient
        className="top-[-500px] dark:opacity-20 opacity-[0.15]"
        conic
        height={1000}
        width={1000}
      />
      <div className="absolute top-0 z-10 w-full h-48 dark:from-black from-white to-transparent bg-gradient-to-b" />
      <FadeIn
        className="z-50 flex flex-col items-center justify-center gap-5 px-6 text-center lg:gap-6"
        delay={0.15}
      >
        <PackLogo
          alt="Turbopack"
          className="w-[160px] md:w-[200px] fill-black dark:fill-white"
          width="200"
        />
        <HeroText h1>The Rust-powered successor to Webpack</HeroText>
        <SectionSubtext hero>
          Turbopack is an incremental bundler optimized for JavaScript and
          TypeScript, written in Rust.
        </SectionSubtext>
      </FadeIn>
      <FadeIn
        className="z-50 flex flex-col items-center w-full max-w-md gap-5 px-6"
        delay={0.3}
      >
        <div className="flex flex-col w-full gap-3 md:!flex-row">
          <CTAButton>
            <Link className="block py-3" href="/pack/docs">
              Get Started
            </Link>
          </CTAButton>
          <CTAButton outline>
            <a
              className="block py-3"
              href="https://github.com/vercel/turbo"
              rel="noreferrer"
              target="_blank"
            >
              GitHub
            </a>
          </CTAButton>
        </div>
        <p className="text-sm text-[#666666]">License: MPL-2.0</p>
      </FadeIn>
      <FadeIn className="relative w-full" delay={0.5}>
        <div className="absolute bottom-0 w-full dark:from-black from-white to-transparent h-72 bg-gradient-to-t" />
      </FadeIn>
    </FadeIn>
  );
}
