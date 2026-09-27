export const auth = $state({
  epoch: 0,
  isLoggedIn: true,
  offline: false,
  user: { id: 1, name: "Example", score_format: "POINT_100" } as {
    id: number;
    name: string;
    score_format: string | null;
  } | null,
});
