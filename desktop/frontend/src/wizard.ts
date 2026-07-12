// Pure helpers for the first-launch onboarding wizard. Ported from
// $REF/desktop/frontend/src/wizard.ts. Sequencing logic kept free of React so it is unit-testable.

export type OnboardStep = "token" | "project";

// onboardingStep sequences the wizard: a Linear credential first, then the project slug that
// seeds WORKFLOW.md (design §6).
export function onboardingStep(hasToken: boolean): OnboardStep {
  return hasToken ? "project" : "token";
}

// slugValid is a light client-side guard before enabling "Create config".
export function slugValid(slug: string): boolean {
  return slug.trim().length > 0;
}
