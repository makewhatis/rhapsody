import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  fetchReviews,
  postReviewDismiss,
  postReviewRerun,
  type ReviewActionResponse,
  type ReviewJob,
  type ReviewsResponse,
} from "@/lib/api";

export const REVIEWS_QUERY_KEY = ["reviews"] as const;

// useReviews polls the ticketless review watch set while the Reviews surface is open, so a round
// starting or finishing shows up without a reload.
//
// `enabled` gates it on the Teams toggle, exactly as every `/api/v1/teams*` hook is gated: the §16
// invariant is that the whole review subsystem is dormant with Teams off, and an app that polled it
// anyway would be making a request about a feature that cannot be running. The daemon would answer
// `{enabled: false, reviews: []}` rather than an error — that answer is for the OTHER half of the
// gate, the review MODE, which nothing on `/api/v1/version` reports.
export function useReviews(enabled: boolean, pollMs?: number) {
  return useQuery<ReviewsResponse>({
    queryKey: REVIEWS_QUERY_KEY,
    queryFn: fetchReviews,
    refetchInterval: pollMs ?? 5000,
    refetchOnWindowFocus: false,
    enabled,
  });
}

// useRerunReview asks the daemon for one more review round of a watched pull request — the trusted
// operator lever (design §15-e), which exists because §14.1's F-SEC finding took this control off
// the team room. On success it invalidates the watch-set query so the row's status moves under the
// operator's hand rather than on the next poll tick.
export function useRerunReview() {
  const qc = useQueryClient();
  return useMutation<ReviewActionResponse, Error, ReviewJob>({
    mutationFn: postReviewRerun,
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: REVIEWS_QUERY_KEY });
    },
  });
}

// useDismissReview drops a pull request out of the watch set. Same refresh, for the same reason:
// the row does not disappear (a retirement is a soft delete) — it re-renders as `dropped`, which is
// the evidence the click landed.
export function useDismissReview() {
  const qc = useQueryClient();
  return useMutation<ReviewActionResponse, Error, ReviewJob>({
    mutationFn: postReviewDismiss,
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: REVIEWS_QUERY_KEY });
    },
  });
}
